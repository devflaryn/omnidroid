//! The MCP server: configuration, the live-instance registry, the lab debug session, and the
//! dispatch of every tool.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Child;

use omni_debug::{HookAction, Session, Stop};

use crate::device;
use crate::json::{self, Json};
use crate::mcp::{Dispatch, RpcError};

/// The protocol version this server speaks. MCP is dated; this is a widely-supported revision.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Configuration, resolved from arguments then environment then defaults. Every field is also
/// overridable per `start_instance` call, so one server can drive several different runs.
#[derive(Debug, Clone)]
pub struct Config {
    /// The APK to boot. `OMNI_MCP_APK` / `--apk`.
    pub apk: Option<PathBuf>,
    /// The cookie file (or saved account name). `OMNI_MCP_COOKIE`.
    pub cookie: Option<String>,
    /// The place id to join. `OMNI_MCP_PLACE`; defaults to PS99.
    pub place: Option<String>,
    /// GPU backend: `vulkan`, `gl` or `auto`. `OMNI_MCP_GPU`.
    pub gpu: String,
    /// Window size `WxH`, or none for the default. `OMNI_MCP_SIZE`.
    pub size: Option<String>,
    /// Device RAM in MiB. `OMNI_MCP_DEVICE_RAM_MB` (passed through as `OMNI_DEVICE_RAM_MB`).
    pub device_ram_mb: Option<u64>,
    /// Minutes to run. `OMNI_MCP_MINUTES`.
    pub minutes: Option<u64>,
    /// The `omnidroid` launcher binary. `OMNIDROID_BIN`, else the sibling of this executable.
    pub omnidroid_bin: PathBuf,
    /// The repository root to run the launcher in (its `aosp` subcommand invokes `cargo test`).
    /// `OMNI_MCP_REPO`, else derived from the executable's path.
    pub repo_dir: Option<PathBuf>,
    /// Keep a standby instance: booted (from the saved device), signed in and -- with a configured
    /// place -- in that place, waiting for the next `start_instance`, which takes it over instead of
    /// booting. It outlives this server, so the next session finds it waiting. `OMNI_MCP_STANDBY=1`.
    pub standby: bool,
    /// How long a standby instance lives, in minutes. `OMNI_MCP_STANDBY_MINUTES` (default 720).
    pub standby_minutes: u64,
    /// Boot the host's warm device (`crate::device`) when a client connects, if none is up, so the
    /// first APK finds it booted. `OMNI_MCP_WARM=1`. Without it the first `start_instance` or
    /// `install_apk` with no account boots it, and it stays up for the next.
    pub warm: bool,
    /// How long a warm device lives, in minutes. `OMNI_MCP_WARM_MINUTES` (default 720).
    pub warm_minutes: u64,
    /// How long a call waits for a warm device still booting, in seconds. `OMNI_MCP_WARM_WAIT`
    /// (default 540: under the client's 600 s call timeout).
    pub warm_wait: u64,
}

impl Config {
    /// Build a config from the environment and sensible defaults.
    #[must_use]
    pub fn from_env() -> Self {
        let exe = std::env::current_exe().ok();
        let omnidroid_bin = std::env::var_os("OMNIDROID_BIN").map(PathBuf::from).unwrap_or_else(|| {
            let name = if cfg!(windows) { "omnidroid.exe" } else { "omnidroid" };
            exe.as_ref()
                .and_then(|e| e.parent())
                .map(|d| d.join(name))
                .unwrap_or_else(|| PathBuf::from(name))
        });
        // The repo is three levels up from target/<profile>/omni-mcp.
        let repo_dir = std::env::var_os("OMNI_MCP_REPO").map(PathBuf::from).or_else(|| {
            exe.as_ref()
                .and_then(|e| e.parent()?.parent()?.parent())
                .map(Path::to_path_buf)
        });
        Self {
            apk: std::env::var_os("OMNI_MCP_APK").map(PathBuf::from),
            cookie: std::env::var("OMNI_MCP_COOKIE").ok(),
            place: std::env::var("OMNI_MCP_PLACE").ok(),
            gpu: std::env::var("OMNI_MCP_GPU").unwrap_or_else(|_| "auto".into()),
            size: std::env::var("OMNI_MCP_SIZE").ok(),
            device_ram_mb: std::env::var("OMNI_MCP_DEVICE_RAM_MB").ok().and_then(|v| v.parse().ok()),
            minutes: std::env::var("OMNI_MCP_MINUTES").ok().and_then(|v| v.parse().ok()),
            omnidroid_bin,
            repo_dir,
            standby: std::env::var("OMNI_MCP_STANDBY").as_deref() == Ok("1"),
            standby_minutes: std::env::var("OMNI_MCP_STANDBY_MINUTES").ok().and_then(|v| v.parse().ok()).unwrap_or(720),
            warm: std::env::var("OMNI_MCP_WARM").as_deref() == Ok("1"),
            warm_minutes: std::env::var("OMNI_MCP_WARM_MINUTES").ok().and_then(|v| v.parse().ok()).unwrap_or(720),
            warm_wait: std::env::var("OMNI_MCP_WARM_WAIT").ok().and_then(|v| v.parse().ok()).unwrap_or(540),
        }
    }
}

/// One booted omnidroid instance.
struct Instance {
    id: String,
    /// The launcher this server started; none for a standby instance another server started.
    child: Option<Child>,
    /// The instance's directory (`<dir>/data/local/tmp` holds its state; `<dir>.log` its log).
    dir: PathBuf,
    /// Taken over from standby: given back (still running) when this server exits.
    standby: bool,
    screenshot: PathBuf,
    apk: Option<PathBuf>,
    place: Option<String>,
    started: std::time::Instant,
    /// On the warm device: the app's package (its device is `dir`, which outlives the instance).
    package: Option<String>,
}

/// The MCP server.
pub struct Server {
    config: Config,
    instances: BTreeMap<String, Instance>,
    next_instance: u64,
    lab: Option<Session>,
    lab_source: Option<String>,
    /// The standby instance this server started, while it has not been taken over.
    standby_child: Option<(Child, PathBuf)>,
}

/// The prefix of a standby instance's directory in the temp directory (`<prefix><unix secs>`).
const STANDBY_PREFIX: &str = "omni-linux-r-standby-";

/// A file in the instance's `/data/local/tmp`: its state, as `r_roblox` keeps it.
fn state_file(dir: &Path, name: &str) -> PathBuf {
    dir.join("data/local/tmp").join(name)
}

/// Whether an instance's session still runs: its log written in the last 90 s (the device's log
/// never goes quiet that long) and no stop asked for.
fn alive(dir: &Path) -> bool {
    let fresh = std::fs::metadata(dir.with_extension("log"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < std::time::Duration::from_secs(90));
    fresh && !state_file(dir, "stop").exists()
}

/// A live device of an app session (`omni-linux-r-*`: a standby or a session any server started),
/// if one runs on this host.
fn live_session() -> Option<PathBuf> {
    std::fs::read_dir(std::env::temp_dir())
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("omni-linux-r-") && !name.ends_with(".ctl") && e.path().is_dir()
        })
        .map(|e| e.path())
        .find(|d| alive(d))
}

/// Seconds, to the millisecond.
fn secs(s: f64) -> Json {
    Json::Num((s * 1000.0).round() / 1000.0)
}

/// A running standby instance nobody has taken over, if there is one (the newest).
fn waiting_standby() -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(std::env::temp_dir())
        .ok()?
        .flatten()
        .filter(|e| device::numbered(&e.file_name().to_string_lossy(), STANDBY_PREFIX) && e.path().is_dir())
        .map(|e| e.path())
        .filter(|d| alive(d) && !state_file(d, "claimed").exists())
        .collect();
    found.sort();
    found.pop()
}

/// What an instance has reached, from the files its session keeps: `booting`, `signed_in`,
/// `joining`, `in_game` (with the place), `stopped`.
fn instance_state(dir: &Path) -> (&'static str, Option<String>) {
    if !alive(dir) {
        return ("stopped", None);
    }
    if let Ok(place) = std::fs::read_to_string(state_file(dir, "game-loaded")) {
        return ("in_game", Some(place.trim().to_string()));
    }
    if state_file(dir, "joining").exists() {
        ("joining", None)
    } else if state_file(dir, "signed-in").exists() {
        ("signed_in", None)
    } else {
        ("booting", None)
    }
}

impl Server {
    /// Create a server with the given configuration.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            instances: BTreeMap::new(),
            next_instance: 1,
            lab: None,
            lab_source: None,
            standby_child: None,
        }
    }

    /// The launcher's command for a session in `dir`: the APK, the account, the place and the
    /// display as given.
    #[allow(clippy::too_many_arguments)]
    fn launcher(&self, dir: &Path, apk: &Path, cookie: Option<&str>, place: Option<&str>, gpu: &str, size: Option<&str>, minutes: Option<u64>) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.config.omnidroid_bin);
        cmd.arg("aosp").arg("--apk").arg(apk).arg("--gpu").arg(gpu).arg("--instance").arg(dir);
        if let Some(c) = cookie {
            cmd.arg("--cookie").arg(c);
        }
        if let Some(p) = place {
            cmd.arg("--place").arg(p);
        }
        if let Some(sz) = size {
            cmd.arg("--size").arg(sz);
        }
        if let Some(m) = minutes {
            cmd.arg("--minutes").arg(m.to_string());
        }
        // The screenshot path we can read (r_roblox honours a preset OMNI_SCREENSHOT).
        cmd.env("OMNI_SCREENSHOT", dir.with_extension("png"));
        if let Some(ram) = self.config.device_ram_mb {
            cmd.env("OMNI_DEVICE_RAM_MB", ram.to_string());
        }
        if let Some(repo) = &self.config.repo_dir {
            cmd.current_dir(repo);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd
    }

    /// With `standby` configured, boot a standby instance unless one runs (this server's, still
    /// booting, or any waiting one).
    fn ensure_standby(&mut self) {
        if !self.config.standby {
            return;
        }
        if let Some((child, _)) = &mut self.standby_child {
            if matches!(child.try_wait(), Ok(None)) {
                return;
            }
            self.standby_child = None;
        }
        if waiting_standby().is_some() {
            return;
        }
        let Some(apk) = self.config.apk.clone().filter(|a| a.is_file()) else { return };
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let dir = std::env::temp_dir().join(format!("{STANDBY_PREFIX}{secs}"));
        let mut cmd = self.launcher(
            &dir,
            &apk,
            self.config.cookie.as_deref(),
            self.config.place.as_deref(),
            &self.config.gpu,
            self.config.size.as_deref(),
            Some(self.config.standby_minutes),
        );
        cmd.arg("--standby");
        if let Ok(child) = cmd.spawn() {
            self.standby_child = Some((child, dir));
        }
    }

    /// Take over a waiting standby instance (or this server's, still booting) for `place`: the
    /// place is joined unless it is the one the standby is in or is on its way into.
    fn claim_standby(&mut self, place: Option<&str>) -> Option<(String, PathBuf, &'static str)> {
        let dir = match self.standby_child.take() {
            // This server's own: from now on an instance like the others (its launcher left running).
            Some((_child, dir)) if !state_file(&dir, "claimed").exists() => dir,
            _ => waiting_standby()?,
        };
        let _ = std::fs::create_dir_all(state_file(&dir, ""));
        let _ = std::fs::write(state_file(&dir, "claimed"), "1");
        let loaded = std::fs::read_to_string(state_file(&dir, "game-loaded")).ok().map(|p| p.trim().to_string());
        let note = match place {
            Some(p) if loaded.as_deref() == Some(p) => "standby taken over: already in the place",
            Some(p) if loaded.is_none() && self.config.place.as_deref() == Some(p) => "standby taken over: on its way into the place",
            Some(p) => {
                // Not in the place asked for until it has loaded that one.
                let _ = std::fs::remove_file(state_file(&dir, "game-loaded"));
                let _ = std::fs::remove_file(state_file(&dir, "joining"));
                let _ = std::fs::write(state_file(&dir, "join-place"), p);
                "standby taken over: joining the place"
            }
            None => "standby taken over",
        };
        let id = format!("inst-{}", self.next_instance);
        self.next_instance += 1;
        Some((id, dir, note))
    }

    // ---- the warm device ----------------------------------------------------------------------

    /// Boot a warm device in `dir`, detached: it outlives this server (the next session finds it).
    fn boot_warm(&self, dir: &Path) -> Result<(), String> {
        // Another device on the host (a session or a standby of this or another server): one
        // Android at a time.
        if let Some(other) = live_session() {
            return Err(format!("another device runs on this host ({}); stop it first (stop_instance, or its data/local/tmp/stop)", other.display()));
        }
        let mut cmd = std::process::Command::new(&self.config.omnidroid_bin);
        cmd.arg("aosp").arg("--warm").arg("--instance").arg(dir).arg("--gpu").arg(&self.config.gpu).arg("--minutes").arg(self.config.warm_minutes.to_string());
        if let Some(sz) = &self.config.size {
            cmd.arg("--size").arg(sz);
        }
        cmd.env("OMNI_SCREENSHOT", dir.with_extension("png"));
        // A frame a second: the agent's screenshot shows what the app just did.
        cmd.env("OMNI_SCREENSHOT_MS", std::env::var("OMNI_MCP_SCREENSHOT_MS").unwrap_or_else(|_| "1000".into()));
        if let Some(ram) = self.config.device_ram_mb {
            cmd.env("OMNI_DEVICE_RAM_MB", ram.to_string());
        }
        if let Some(repo) = &self.config.repo_dir {
            cmd.current_dir(repo);
        }
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        cmd.spawn().map(|_| ()).map_err(|e| format!("could not start {}: {e}", self.config.omnidroid_bin.display()))
    }

    /// The warm device, ready: the live one, or one booted now (waited for, `warm_wait` at most).
    /// With it, how it was found (`warm`, `booted`, `waited`) and the seconds waited.
    fn warm_device(&self) -> Result<(device::Device, &'static str, f64), RpcError> {
        let t = std::time::Instant::now();
        let booted = std::cell::Cell::new(false);
        let found = device::ensure(|dir| {
            booted.set(true);
            self.boot_warm(dir)
        })
        .map_err(RpcError::server)?;
        let d = match found {
            device::Found::Ready(d) => return Ok((d, "warm", t.elapsed().as_secs_f64())),
            device::Found::Booting(d) => d,
        };
        let how = if booted.get() { "booted" } else { "waited" };
        device::wait_ready(&d, std::time::Duration::from_secs(self.config.warm_wait)).map_err(RpcError::server)?;
        Ok((d, how, t.elapsed().as_secs_f64()))
    }

    /// With `warm` configured, boot the warm device now unless one is up or booting.
    fn ensure_warm(&self) {
        if self.config.warm {
            let _ = device::ensure(|dir| self.boot_warm(dir));
        }
    }

    /// `start_instance` without an account: the APK on the warm device -- installed unless the
    /// device holds the same bytes, and started -- answered once its Activity is displayed.
    fn start_on_warm(&mut self, apk: PathBuf, launch: bool) -> Result<Json, RpcError> {
        let t = std::time::Instant::now();
        let app = device::Apk::read(&apk).map_err(RpcError::params)?;
        let read_s = t.elapsed().as_secs_f64();
        let (dev, how, device_s) = self.warm_device()?;
        let installed = device::install(&dev, &app).map_err(RpcError::server)?;
        // Instances of apps this install replaced are over.
        self.instances.retain(|_, i| i.package.as_ref().map_or(true, |p| !installed.uninstalled.contains(p)));
        let mut out = vec![
            ("package", json::s(app.package.clone())),
            ("sha256", json::s(app.sha256.clone())),
            ("version_code", app.version_code.map_or(Json::Null, |v| Json::Num(f64::from(v)))),
            ("device", json::obj([("dir", json::s(dev.dir.to_string_lossy().into_owned())), ("how", json::s(how)), ("seconds", secs(device_s))])),
            (
                "install",
                json::obj([
                    ("action", json::s(installed.action)),
                    ("seconds", secs(installed.seconds)),
                    ("uninstalled", Json::Array(installed.uninstalled.iter().map(|p| json::s(p.clone())).collect())),
                ]),
            ),
        ];
        if launch {
            let started = device::start(&dev, &app, std::time::Duration::from_secs(300)).map_err(RpcError::server)?;
            let id = format!("inst-{}", self.next_instance);
            self.next_instance += 1;
            // One instance per app: a start of the same app again is that instance.
            self.instances.retain(|_, i| i.package.as_deref() != Some(app.package.as_str()));
            self.instances.insert(
                id.clone(),
                Instance {
                    id: id.clone(),
                    child: None,
                    dir: dev.dir.clone(),
                    standby: false,
                    screenshot: dev.screenshot(),
                    apk: Some(apk),
                    place: None,
                    started: std::time::Instant::now(),
                    package: Some(app.package.clone()),
                },
            );
            out.insert(0, ("instance_id", json::s(id)));
            out.insert(1, ("state", json::s("app_on_screen")));
            out.push((
                "start",
                json::obj([
                    ("component", json::s(started.component)),
                    ("seconds", secs(started.seconds)),
                    ("total_time_ms", started.total_time_ms.map_or(Json::Null, |v| Json::Num(v as f64))),
                    ("status", json::s(started.status)),
                ]),
            ));
            out.push(("screenshot_path", json::s(dev.screenshot().to_string_lossy().into_owned())));
        }
        out.push(("apk_read_seconds", secs(read_s)));
        out.push(("seconds", secs(t.elapsed().as_secs_f64())));
        Ok(Json::Object(out.into_iter().map(|(k, v)| (k.to_string(), v)).collect()))
    }

    /// The live warm device, or an error saying there is none.
    fn live_warm() -> Result<device::Device, RpcError> {
        device::find().filter(device::Device::ready).ok_or_else(|| RpcError::server("no warm device is up (start_instance or install_apk with an APK boots one)"))
    }

    fn shell(&mut self, args: &Json) -> Result<Json, RpcError> {
        let command = args.get("command").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `command`"))?;
        let uid = args.get("uid").and_then(Json::as_u64).map(|u| u as u32);
        let limit = args.get("timeout").and_then(Json::as_u64).unwrap_or(120);
        let dev = Self::live_warm()?;
        let t = std::time::Instant::now();
        let (code, output) = dev.shell_as(command, uid, std::time::Duration::from_secs(limit)).map_err(RpcError::server)?;
        Ok(json::obj([("exit", Json::Num(code as f64)), ("output", json::s(output)), ("seconds", secs(t.elapsed().as_secs_f64()))]))
    }

    fn uninstall_apk(&mut self, args: &Json) -> Result<Json, RpcError> {
        let package = args.get("package").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `package`"))?;
        let dev = Self::live_warm()?;
        let (code, output) = dev.shell(&format!("pm uninstall {package}"), std::time::Duration::from_secs(120)).map_err(RpcError::server)?;
        self.instances.retain(|_, i| i.package.as_deref() != Some(package));
        Ok(json::obj([("package", json::s(package)), ("exit", Json::Num(code as f64)), ("output", json::s(output.trim()))]))
    }

    fn stop_app(&mut self, args: &Json) -> Result<Json, RpcError> {
        let package = args.get("package").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `package`"))?;
        let dev = Self::live_warm()?;
        let output = device::stop_app(&dev, package).map_err(RpcError::server)?;
        Ok(json::obj([("package", json::s(package)), ("output", json::s(output)), ("device", json::s("warm, idle"))]))
    }

    fn device_status(&mut self) -> Json {
        let Some(dev) = device::find() else {
            return json::obj([("state", json::s("none"))]);
        };
        let state = if dev.ready() { "ready" } else { "booting" };
        let apps = if dev.ready() {
            dev.shell("pm list packages -3", std::time::Duration::from_secs(30))
                .map(|(_, out)| Json::Array(out.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(|p| json::s(p)).collect()))
                .unwrap_or(Json::Null)
        } else {
            Json::Null
        };
        json::obj([
            ("state", json::s(state)),
            ("dir", json::s(dev.dir.to_string_lossy().into_owned())),
            ("log", json::s(dev.log().to_string_lossy().into_owned())),
            ("screenshot_path", json::s(dev.screenshot().to_string_lossy().into_owned())),
            ("test_apps", apps),
        ])
    }

    fn stop_device(&mut self) -> Json {
        let Some(dev) = device::find() else {
            return json::obj([("stopped", Json::Bool(false)), ("note", json::s("no warm device is up"))]);
        };
        dev.stop();
        self.instances.retain(|_, i| i.package.is_none());
        json::obj([("stopped", Json::Bool(true)), ("dir", json::s(dev.dir.to_string_lossy().into_owned()))])
    }

    // ---- helpers ------------------------------------------------------------------------------

    fn lab_mut(&mut self) -> Result<&mut Session, RpcError> {
        self.lab
            .as_mut()
            .ok_or_else(|| RpcError::server("no lab session; call `lab_load` with a library path first"))
    }

    /// Resolve a call/breakpoint target given as either `symbol` or `address` in `args`.
    fn target_address(lab: &Session, args: &Json) -> Result<usize, RpcError> {
        if let Some(sym) = args.get("symbol").and_then(Json::as_str) {
            let info = lab.resolve_symbol(sym).map_err(|e| RpcError::params(e.to_string()))?;
            Ok(info.address)
        } else if let Some(addr) = args.get("address").and_then(Json::as_u64) {
            Ok(addr as usize)
        } else {
            Err(RpcError::params("need `symbol` or `address`"))
        }
    }

    // ---- tool catalogue -----------------------------------------------------------------------

    fn tool_list() -> Json {
        // (name, description, inputSchema properties as (name, type, required, desc))
        let tools = TOOLS.iter().map(|t| {
            let props: BTreeMap<String, Json> = t
                .params
                .iter()
                .map(|p| {
                    (
                        p.name.to_string(),
                        json::obj([("type", json::s(p.ty)), ("description", json::s(p.desc))]),
                    )
                })
                .collect();
            let required: Vec<Json> =
                t.params.iter().filter(|p| p.required).map(|p| json::s(p.name)).collect();
            json::obj([
                ("name", json::s(t.name)),
                ("description", json::s(t.desc)),
                (
                    "inputSchema",
                    json::obj([
                        ("type", json::s("object")),
                        ("properties", Json::Object(props)),
                        ("required", Json::Array(required)),
                    ]),
                ),
            ])
        });
        json::obj([("tools", Json::Array(tools.collect()))])
    }

    // ---- dispatch of tools/call ---------------------------------------------------------------

    fn call_tool(&mut self, name: &str, args: &Json) -> Result<Json, RpcError> {
        let result = match name {
            // lifecycle / session (live)
            "start_instance" => self.start_instance(args),
            "stop_instance" => self.stop_instance(args),
            "list_instances" => Ok(self.list_instances()),
            "install_apk" => self.install_apk(args),
            "launch_app" => self.start_instance(args), // launch == boot on this path
            "login" => self.login(args),
            "join_place" => self.join_place(args),
            "screenshot" => self.screenshot(args),
            // the warm device
            "shell" => self.shell(args),
            "uninstall_apk" => self.uninstall_apk(args),
            "stop_app" => self.stop_app(args),
            "device_status" => Ok(self.device_status()),
            "stop_device" => Ok(self.stop_device()),
            // lab (emulation-layer debug/dump)
            "lab_load" => self.lab_load(args),
            "resolve_symbol" => self.resolve_symbol(args),
            "list_symbols" => self.list_symbols(args),
            "list_maps" => self.list_maps(),
            "read_mem" => self.read_mem(args),
            "write_mem" => self.write_mem(args),
            "dump_module" => self.dump_module(args),
            "call_function" => self.call_function(args),
            "set_breakpoint" => self.set_breakpoint(args),
            "run_until_stop" => self.run_until_stop(args),
            "resume" => self.resume(),
            "get_registers" => self.get_registers(),
            "backtrace" => self.backtrace(),
            "intercept" => self.intercept(args),
            "trace_syscalls" => self.trace_syscalls(args),
            "alloc_data" => self.alloc_data(args),
            "load_code" => self.load_code(args),
            _ => return Err(RpcError { code: crate::mcp::code::METHOD_NOT_FOUND, message: format!("no tool named {name}") }),
        };
        result.map(text_result)
    }

    // ---- live tools ---------------------------------------------------------------------------

    fn start_instance(&mut self, args: &Json) -> Result<Json, RpcError> {
        let apk = args
            .get("apk")
            .and_then(Json::as_str)
            .map(PathBuf::from)
            .or_else(|| self.config.apk.clone())
            .ok_or_else(|| RpcError::params("no APK: pass `apk` or set OMNI_MCP_APK"))?;
        if !apk.is_file() {
            return Err(RpcError::params(format!("APK not found: {}", apk.display())));
        }
        let cookie = args.get("cookie").and_then(Json::as_str).map(str::to_string).or_else(|| self.config.cookie.clone());
        let place = args.get("place").and_then(Json::as_str).map(str::to_string).or_else(|| self.config.place.clone());
        let gpu = args.get("gpu").and_then(Json::as_str).map(str::to_string).unwrap_or_else(|| self.config.gpu.clone());
        let size = args.get("size").and_then(Json::as_str).map(str::to_string).or_else(|| self.config.size.clone());
        let minutes = args.get("minutes").and_then(Json::as_u64).or(self.config.minutes);

        // No account: the APK on the host's warm device, installed by content and started.
        if cookie.is_none() {
            return self.start_on_warm(apk, true);
        }

        // A standby instance of the same APK and account is taken over: no boot at all.
        let same_setup = Some(apk.as_path()) == self.config.apk.as_deref()
            && cookie == self.config.cookie
            && args.get("gpu").is_none()
            && args.get("size").is_none();
        if self.config.standby && same_setup {
            if let Some((id, dir, note)) = self.claim_standby(place.as_deref()) {
                let screenshot = dir.with_extension("png");
                let (state, in_place) = instance_state(&dir);
                self.instances.insert(
                    id.clone(),
                    Instance {
                        id: id.clone(),
                        child: None,
                        dir,
                        standby: true,
                        screenshot: screenshot.clone(),
                        apk: Some(apk),
                        place,
                        started: std::time::Instant::now(),
                        package: None,
                    },
                );
                return Ok(json::obj([
                    ("instance_id", json::s(id)),
                    ("state", json::s(state)),
                    ("in_place", in_place.map_or(Json::Null, json::s)),
                    ("screenshot_path", json::s(screenshot.to_string_lossy().into_owned())),
                    ("note", json::s(format!("{note}; `list_instances` shows its state (in_game once the place has loaded)"))),
                ]));
            }
        }

        let id = format!("inst-{}", self.next_instance);
        self.next_instance += 1;
        let dir = std::env::temp_dir().join(format!("omni-linux-r-mcp-{}-{id}", std::process::id()));
        let screenshot = dir.with_extension("png");
        let _ = std::fs::remove_file(&screenshot);
        let mut cmd = self.launcher(&dir, &apk, cookie.as_deref(), place.as_deref(), &gpu, size.as_deref(), minutes);
        let child = cmd.spawn().map_err(|e| {
            RpcError::server(format!("could not start {}: {e}", self.config.omnidroid_bin.display()))
        })?;
        let pid = child.id();
        let instance = Instance {
            id: id.clone(),
            child: Some(child),
            dir,
            standby: false,
            screenshot: screenshot.clone(),
            apk: Some(apk),
            place: place.clone(),
            started: std::time::Instant::now(),
            package: None,
        };
        self.instances.insert(id.clone(), instance);

        Ok(json::obj([
            ("instance_id", json::s(id)),
            ("pid", Json::Num(pid as f64)),
            ("screenshot_path", json::s(screenshot.to_string_lossy().into_owned())),
            ("state", json::s("booting")),
            ("note", json::s("booting; `list_instances` shows its state (in_game once the place has loaded)")),
        ]))
    }

    fn stop_instance(&mut self, args: &Json) -> Result<Json, RpcError> {
        let id = args.get("instance_id").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `instance_id`"))?;
        let mut inst = self.instances.remove(id).ok_or_else(|| RpcError::params(format!("no instance {id}")))?;
        // An app on the warm device: stopped and its data cleared; the device stays up for the next.
        if let Some(package) = &inst.package {
            let dev = device::Device { dir: inst.dir.clone() };
            let output = if dev.alive() { device::stop_app(&dev, package).unwrap_or_else(|e| e) } else { "the device is gone".into() };
            if args.get("device").and_then(Json::as_bool) == Some(true) {
                dev.stop();
            }
            return Ok(json::obj([("instance_id", json::s(id)), ("stopped", Json::Bool(true)), ("package", json::s(package.clone())), ("output", json::s(output))]));
        }
        stop(&mut inst);
        // A standby taken over and stopped: the next one boots now.
        if inst.standby {
            self.ensure_standby();
        }
        Ok(json::obj([("instance_id", json::s(id)), ("stopped", Json::Bool(true))]))
    }

    fn list_instances(&mut self) -> Json {
        let mut out = Vec::new();
        for inst in self.instances.values_mut() {
            let (state, in_place) = if inst.package.is_some() {
                (if (device::Device { dir: inst.dir.clone() }).alive() { "app_running" } else { "stopped" }, None)
            } else {
                instance_state(&inst.dir)
            };
            // Until its session writes a log, a launcher still running is booting.
            let launching = inst.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None))) && !inst.dir.with_extension("log").exists();
            let state = if launching { "booting" } else { state };
            out.push(json::obj([
                ("instance_id", json::s(inst.id.clone())),
                ("pid", inst.child.as_ref().map_or(Json::Null, |c| Json::Num(c.id() as f64))),
                ("running", Json::Bool(state != "stopped")),
                ("state", json::s(state)),
                ("in_place", in_place.map_or(Json::Null, json::s)),
                ("log", json::s(inst.dir.with_extension("log").to_string_lossy().into_owned())),
                ("uptime_s", Json::Num(inst.started.elapsed().as_secs() as f64)),
                ("apk", inst.apk.as_ref().map(|a| json::s(a.to_string_lossy().into_owned())).unwrap_or(Json::Null)),
                ("place", inst.place.as_ref().map(|p| json::s(p.clone())).unwrap_or(Json::Null)),
                ("screenshot_path", json::s(inst.screenshot.to_string_lossy().into_owned())),
            ]));
        }
        json::obj([("instances", Json::Array(out))])
    }

    fn install_apk(&mut self, args: &Json) -> Result<Json, RpcError> {
        let apk = args.get("apk").and_then(Json::as_str).map(PathBuf::from).ok_or_else(|| RpcError::params("need `apk`"))?;
        if !apk.is_file() {
            return Err(RpcError::params(format!("APK not found: {}", apk.display())));
        }
        // The default APK for the next start, and installed now on the warm device (booted if none
        // is up) -- or kept as it is when the device holds the same bytes.
        self.config.apk = Some(apk.clone());
        self.start_on_warm(apk, false)
    }

    fn login(&mut self, args: &Json) -> Result<Json, RpcError> {
        let cookie = args.get("cookie").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `cookie` (a file path or a saved account name)"))?;
        self.config.cookie = Some(cookie.to_string());
        Ok(json::obj([("cookie", json::s(cookie)), ("note", json::s("recorded; applied at the next start_instance/launch_app"))]))
    }

    fn join_place(&mut self, args: &Json) -> Result<Json, RpcError> {
        let place = args.get("place").and_then(Json::as_str).or_else(|| args.get("id").and_then(Json::as_str)).ok_or_else(|| RpcError::params("need `place`"))?;
        self.config.place = Some(place.to_string());
        Ok(json::obj([("place", json::s(place)), ("note", json::s("recorded; applied at the next start_instance/launch_app"))]))
    }

    fn screenshot(&mut self, args: &Json) -> Result<Json, RpcError> {
        // Which instance's framebuffer to read.
        let path = if let Some(id) = args.get("instance_id").and_then(Json::as_str) {
            self.instances.get(id).map(|i| i.screenshot.clone()).ok_or_else(|| RpcError::params(format!("no instance {id}")))?
        } else if let Some(p) = args.get("path").and_then(Json::as_str) {
            PathBuf::from(p)
        } else if let Some(inst) = self.instances.values().next_back() {
            inst.screenshot.clone()
        } else {
            return Err(RpcError::params("no instance and no `path`"));
        };
        if !path.is_file() {
            return Err(RpcError::server(format!("no frame yet at {} (still booting?)", path.display())));
        }
        let bytes = std::fs::read(&path).map_err(|e| RpcError::server(format!("read {}: {e}", path.display())))?;
        let (w, h) = png_dimensions(&bytes).unwrap_or((0, 0));
        Ok(json::obj([
            ("path", json::s(path.to_string_lossy().into_owned())),
            ("bytes", Json::Num(bytes.len() as f64)),
            ("width", Json::Num(w as f64)),
            ("height", Json::Num(h as f64)),
        ]))
    }

    // ---- lab tools ----------------------------------------------------------------------------

    fn lab_load(&mut self, args: &Json) -> Result<Json, RpcError> {
        let path = args.get("path").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `path` to a library (.so)"))?;
        let name = args
            .get("name")
            .and_then(Json::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| Path::new(path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "module".into()));
        let mut session = Session::new().map_err(|e| RpcError::server(e.to_string()))?;
        session.load(&name, path).map_err(|e| RpcError::server(e.to_string()))?;
        let (base, start, end) = session.module_span(&name).map_err(|e| RpcError::server(e.to_string()))?;
        let symbols = session.list_symbols(&name).map(|v| v.len()).unwrap_or(0);
        self.lab = Some(session);
        self.lab_source = Some(name.clone());
        Ok(json::obj([
            ("module", json::s(name)),
            ("base", hex(base as u64)),
            ("start", hex(start as u64)),
            ("end", hex(end as u64)),
            ("exported_symbols", Json::Num(symbols as f64)),
        ]))
    }

    fn resolve_symbol(&mut self, args: &Json) -> Result<Json, RpcError> {
        let name = args.get("name").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `name`"))?;
        let lab = self.lab_mut()?;
        let info = lab.resolve_symbol(name).map_err(|e| RpcError::server(e.to_string()))?;
        Ok(json::obj([
            ("name", json::s(info.name)),
            ("address", hex(info.address as u64)),
            ("size", Json::Num(info.size as f64)),
            ("kind", json::s(info.kind)),
            ("module", json::s(info.module)),
        ]))
    }

    fn list_symbols(&mut self, args: &Json) -> Result<Json, RpcError> {
        let module = args.get("module").and_then(Json::as_str).map(str::to_string);
        let lab = self.lab_mut()?;
        let module = module.or_else(|| lab.modules().into_iter().next()).ok_or_else(|| RpcError::params("no module loaded"))?;
        let syms = lab.list_symbols(&module).map_err(|e| RpcError::server(e.to_string()))?;
        let limit = args.get("limit").and_then(Json::as_u64).unwrap_or(200) as usize;
        let out: Vec<Json> = syms
            .into_iter()
            .take(limit)
            .map(|s| json::obj([("name", json::s(s.name)), ("address", hex(s.address as u64)), ("kind", json::s(s.kind)), ("size", Json::Num(s.size as f64))]))
            .collect();
        Ok(json::obj([("module", json::s(module)), ("symbols", Json::Array(out))]))
    }

    fn list_maps(&mut self) -> Result<Json, RpcError> {
        let lab = self.lab_mut()?;
        let maps: Vec<Json> = lab
            .list_maps()
            .into_iter()
            .map(|m| {
                json::obj([
                    ("start", hex(m.start as u64)),
                    ("end", hex(m.end as u64)),
                    ("perms", json::s(m.perms_str())),
                    ("committed", Json::Num(m.committed as f64)),
                    ("what", json::s(m.what)),
                ])
            })
            .collect();
        Ok(json::obj([("maps", Json::Array(maps))]))
    }

    fn read_mem(&mut self, args: &Json) -> Result<Json, RpcError> {
        let addr = args.get("address").and_then(Json::as_u64).ok_or_else(|| RpcError::params("need `address`"))? as usize;
        let len = args.get("len").and_then(Json::as_u64).ok_or_else(|| RpcError::params("need `len`"))? as usize;
        if len > 1 << 20 {
            return Err(RpcError::params("len too large (max 1 MiB per read)"));
        }
        let lab = self.lab_mut()?;
        let bytes = lab.read_mem(addr, len).map_err(|e| RpcError::server(e.to_string()))?;
        Ok(json::obj([("address", hex(addr as u64)), ("len", Json::Num(len as f64)), ("hex", json::s(to_hex(&bytes)))]))
    }

    fn write_mem(&mut self, args: &Json) -> Result<Json, RpcError> {
        let addr = args.get("address").and_then(Json::as_u64).ok_or_else(|| RpcError::params("need `address`"))? as usize;
        let hex_in = args.get("hex").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `hex` (the bytes as hex)"))?;
        let bytes = from_hex(hex_in).map_err(RpcError::params)?;
        let lab = self.lab_mut()?;
        lab.write_mem(addr, &bytes).map_err(|e| RpcError::server(e.to_string()))?;
        Ok(json::obj([("address", hex(addr as u64)), ("written", Json::Num(bytes.len() as f64))]))
    }

    fn dump_module(&mut self, args: &Json) -> Result<Json, RpcError> {
        let name = args.get("name").and_then(Json::as_str).map(str::to_string);
        let out_path = args.get("out_path").and_then(Json::as_str).map(PathBuf::from);
        let lab = self.lab_mut()?;
        let name = name.or_else(|| lab.modules().into_iter().next()).ok_or_else(|| RpcError::params("no module loaded"))?;
        let bytes = lab.dump_module(&name).map_err(|e| RpcError::server(e.to_string()))?;
        let mut result = vec![("module", json::s(name.clone())), ("bytes", Json::Num(bytes.len() as f64))];
        if let Some(path) = out_path {
            std::fs::write(&path, &bytes).map_err(|e| RpcError::server(format!("write {}: {e}", path.display())))?;
            result.push(("out_path", json::s(path.to_string_lossy().into_owned())));
        }
        Ok(Json::Object(result.into_iter().map(|(k, v)| (k.to_string(), v)).collect()))
    }

    fn call_function(&mut self, args: &Json) -> Result<Json, RpcError> {
        let call_args: Vec<u64> = match args.get("args") {
            Some(Json::Array(a)) => a
                .iter()
                .map(|v| v.as_u64().ok_or_else(|| RpcError::params("each arg must be a non-negative integer or a hex/decimal string")))
                .collect::<Result<_, _>>()?,
            None => Vec::new(),
            _ => return Err(RpcError::params("`args` must be an array")),
        };
        let lab = self.lab.as_ref().ok_or_else(|| RpcError::server("no lab session"))?;
        let addr = Self::target_address(lab, args)?;
        let lab = self.lab_mut()?;
        let out = lab.call_function(addr, &call_args).map_err(|e| RpcError::server(e.to_string()))?;
        Ok(json::obj([
            ("ret", hex(out.ret)),
            ("ret_u64", Json::Num(out.ret as f64)),
            ("ret1", hex(out.ret1)),
            ("instructions", Json::Num(out.instructions as f64)),
            ("events", events_json(&out.events)),
        ]))
    }

    fn set_breakpoint(&mut self, args: &Json) -> Result<Json, RpcError> {
        let lab = self.lab.as_ref().ok_or_else(|| RpcError::server("no lab session"))?;
        let addr = Self::target_address(lab, args)?;
        let lab = self.lab_mut()?;
        lab.set_breakpoint(addr).map_err(|e| RpcError::server(e.to_string()))?;
        Ok(json::obj([("breakpoint", hex(addr as u64))]))
    }

    fn run_until_stop(&mut self, args: &Json) -> Result<Json, RpcError> {
        let call_args: Vec<u64> = match args.get("args") {
            Some(Json::Array(a)) => a.iter().map(|v| v.as_u64().ok_or_else(|| RpcError::params("bad arg"))).collect::<Result<_, _>>()?,
            _ => Vec::new(),
        };
        let lab = self.lab.as_ref().ok_or_else(|| RpcError::server("no lab session"))?;
        let addr = Self::target_address(lab, args)?;
        let lab = self.lab_mut()?;
        let stop = lab.run_until_stop(addr, &call_args).map_err(|e| RpcError::server(e.to_string()))?;
        Ok(stop_json(&stop))
    }

    fn resume(&mut self) -> Result<Json, RpcError> {
        let lab = self.lab_mut()?;
        let stop = lab.resume().map_err(|e| RpcError::server(e.to_string()))?;
        Ok(stop_json(&stop))
    }

    fn get_registers(&mut self) -> Result<Json, RpcError> {
        let lab = self.lab_mut()?;
        let r = lab.get_registers();
        let xs: Vec<Json> = r.x.iter().map(|&v| hex(v)).collect();
        Ok(json::obj([
            ("x", Json::Array(xs)),
            ("sp", hex(r.sp as u64)),
            ("pc", hex(r.pc as u64)),
            ("nzcv", hex(r.nzcv as u64)),
            ("tpidr_el0", hex(r.tpidr_el0 as u64)),
        ]))
    }

    fn backtrace(&mut self) -> Result<Json, RpcError> {
        let lab = self.lab_mut()?;
        let frames: Vec<Json> = lab.backtrace().into_iter().map(|a| hex(a as u64)).collect();
        Ok(json::obj([("frames", Json::Array(frames))]))
    }

    fn intercept(&mut self, args: &Json) -> Result<Json, RpcError> {
        let record_entry = args.get("record_entry").and_then(Json::as_bool).unwrap_or(true);
        let record_exit = args.get("record_exit").and_then(Json::as_bool).unwrap_or(true);
        let replace_return = args.get("replace_return").and_then(Json::as_u64);
        let lab = self.lab.as_ref().ok_or_else(|| RpcError::server("no lab session"))?;
        let addr = Self::target_address(lab, args)?;
        let lab = self.lab_mut()?;
        lab.intercept(addr, HookAction { record_entry, record_exit, replace_return })
            .map_err(|e| RpcError::server(e.to_string()))?;
        Ok(json::obj([("intercept", hex(addr as u64))]))
    }

    fn trace_syscalls(&mut self, args: &Json) -> Result<Json, RpcError> {
        let on = args.get("on").and_then(Json::as_bool).unwrap_or(true);
        let lab = self.lab_mut()?;
        lab.trace_syscalls(on).map_err(|e| RpcError::server(e.to_string()))?;
        Ok(json::obj([("trace_syscalls", Json::Bool(on))]))
    }

    fn alloc_data(&mut self, args: &Json) -> Result<Json, RpcError> {
        let bytes = if let Some(h) = args.get("hex").and_then(Json::as_str) {
            from_hex(h).map_err(RpcError::params)?
        } else if let Some(n) = args.get("len").and_then(Json::as_u64) {
            vec![0u8; n as usize]
        } else {
            return Err(RpcError::params("need `hex` or `len`"));
        };
        let lab = self.lab_mut()?;
        let addr = lab.alloc_data(&bytes).map_err(|e| RpcError::server(e.to_string()))?;
        Ok(json::obj([("address", hex(addr as u64)), ("len", Json::Num(bytes.len() as f64))]))
    }

    fn load_code(&mut self, args: &Json) -> Result<Json, RpcError> {
        let words: Vec<u32> = match args.get("words") {
            Some(Json::Array(a)) => a
                .iter()
                .map(|v| v.as_u64().filter(|n| *n <= u32::MAX as u64).map(|n| n as u32).ok_or_else(|| RpcError::params("each word must be a 32-bit value")))
                .collect::<Result<_, _>>()?,
            _ => return Err(RpcError::params("need `words` (an array of 32-bit A64 encodings)")),
        };
        let lab = self.lab_mut()?;
        let addr = lab.load_code(&words).map_err(|e| RpcError::server(e.to_string()))?;
        Ok(json::obj([("address", hex(addr as u64))]))
    }
}

impl Dispatch for Server {
    fn handle(&mut self, method: &str, params: &Json) -> Result<Json, RpcError> {
        match method {
            "initialize" => Ok(json::obj([
                ("protocolVersion", json::s(PROTOCOL_VERSION)),
                ("capabilities", json::obj([("tools", json::obj([]))])),
                (
                    "serverInfo",
                    json::obj([("name", json::s("omni-mcp")), ("version", json::s(env!("CARGO_PKG_VERSION")))]),
                ),
            ])),
            "notifications/initialized" | "initialized" => {
                self.ensure_standby();
                self.ensure_warm();
                Ok(Json::Null)
            }
            "ping" => Ok(json::obj([])),
            "tools/list" => Ok(Self::tool_list()),
            "tools/call" => {
                let name = params.get("name").and_then(Json::as_str).ok_or_else(|| RpcError::params("tools/call needs `name`"))?;
                let empty = Json::Object(Default::default());
                let arguments = params.get("arguments").unwrap_or(&empty);
                self.call_tool(name, arguments)
            }
            other => Err(RpcError { code: crate::mcp::code::METHOD_NOT_FOUND, message: format!("no method {other}") }),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Do not leave booted instances running when the server exits -- but a standby taken over
        // is given back, still running (and in its place), for the next session to take.
        for inst in self.instances.values_mut() {
            // An app on the warm device is stopped; the device stays up for the next session.
            if let Some(package) = &inst.package {
                let dev = device::Device { dir: inst.dir.clone() };
                if dev.alive() {
                    let _ = device::stop_app(&dev, package);
                }
                continue;
            }
            if inst.standby {
                let _ = std::fs::remove_file(state_file(&inst.dir, "claimed"));
            } else {
                stop(inst);
            }
        }
    }
}

/// Stop an instance: its session is asked to end (it shuts the device down and removes its
/// directory), and the launcher this server started is ended.
fn stop(inst: &mut Instance) {
    let _ = std::fs::create_dir_all(state_file(&inst.dir, ""));
    let _ = std::fs::write(state_file(&inst.dir, "stop"), "1");
    if let Some(child) = &mut inst.child {
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ---- tool catalogue data ----------------------------------------------------------------------

struct Param {
    name: &'static str,
    ty: &'static str,
    required: bool,
    desc: &'static str,
}

struct Tool {
    name: &'static str,
    desc: &'static str,
    params: &'static [Param],
}

macro_rules! p {
    ($n:literal, $t:literal, $r:literal, $d:literal) => {
        Param { name: $n, ty: $t, required: $r, desc: $d }
    };
}

static TOOLS: &[Tool] = &[
    Tool { name: "start_instance", desc: "Start an APK. Without `cookie` (any APK): on the host's warm device -- one Android kept booted and idle -- the APK is installed unless the device already holds the same bytes (decided by SHA-256, not version; another test app is uninstalled first) and its launcher Activity started; answers once the Activity is displayed (state app_on_screen), with seconds per step. The first call on a host with no warm device boots one (minutes) and it stays up for the next APKs. With `cookie` (a Roblox account): a signed-in session that joins `place`, answered at once (a standby instance is taken over when configured); `list_instances` shows booting, signed_in, joining, in_game.", params: &[
        p!("apk","string",false,"APK path (else the configured default)"),
        p!("cookie","string",false,"cookie file path or saved account name"),
        p!("place","string",false,"place id to join (e.g. 8737899170)"),
        p!("gpu","string",false,"vulkan | gl | auto"),
        p!("size","string",false,"window size WxH"),
        p!("minutes","number",false,"minutes to run"),
    ]},
    Tool { name: "stop_instance", desc: "Stop an instance. An app on the warm device is force-stopped and its data cleared; the device stays warm for the next APK (`device: true` shuts it down too). A session instance's device shuts down.", params: &[p!("instance_id","string",true,"the id from start_instance"),p!("device","boolean",false,"also shut the warm device down")] },
    Tool { name: "list_instances", desc: "List this server's instances: each one's state (booting, signed_in, joining, in_game with the place, stopped) and log.", params: &[] },
    Tool { name: "install_apk", desc: "Install an APK on the warm device now, without starting it (booting the device if none is up): kept as it is when the device holds the same bytes, reinstalled when the bytes differ (even at the same version), another test app uninstalled first. Also the default APK for the next start_instance.", params: &[p!("apk","string",true,"APK path")] },
    Tool { name: "uninstall_apk", desc: "Uninstall a package from the warm device.", params: &[p!("package","string",true,"package name, e.g. com.example.app")] },
    Tool { name: "stop_app", desc: "Force-stop a package on the warm device and clear its data; the device stays warm.", params: &[p!("package","string",true,"package name")] },
    Tool { name: "shell", desc: "Run a shell command on the warm device (as adb shell would: the shell user, or `uid`): its exit status and output. E.g. `pm list packages -3`, `am start ...`, `logcat -d -t 50`, `dumpsys activity top`.", params: &[p!("command","string",true,"a /system/bin/sh command line"),p!("uid","number",false,"run as this uid (0 root, 1000 system; default 2000 shell)"),p!("timeout","number",false,"seconds (default 120)")] },
    Tool { name: "device_status", desc: "The warm device: none, booting or ready; its directory, log, screenshot path and the test apps installed on it.", params: &[] },
    Tool { name: "stop_device", desc: "Shut the warm device down (the next start_instance boots one again).", params: &[] },
    Tool { name: "launch_app", desc: "Boot the app (same as start_instance on the real-AOSP path).", params: &[p!("apk","string",false,"APK path"),p!("cookie","string",false,"cookie"),p!("place","string",false,"place id")] },
    Tool { name: "login", desc: "Record a cookie (file path or saved account name) for the next boot.", params: &[p!("cookie","string",true,"cookie file path or saved account name")] },
    Tool { name: "join_place", desc: "Record a place id to join on the next boot.", params: &[p!("place","string",true,"place id")] },
    Tool { name: "screenshot", desc: "Read the latest framebuffer PNG of an instance; returns the path and dimensions.", params: &[p!("instance_id","string",false,"instance (else the most recent)"),p!("path","string",false,"an explicit PNG path")] },
    Tool { name: "lab_load", desc: "Load a guest arm64 library (.so) into an emulation-layer lab session for debugging and dumping.", params: &[p!("path","string",true,"path to the .so"),p!("name","string",false,"a name for the module")] },
    Tool { name: "resolve_symbol", desc: "Resolve an exported symbol to a guest address in the lab.", params: &[p!("name","string",true,"symbol name")] },
    Tool { name: "list_symbols", desc: "List a module's exported symbols.", params: &[p!("module","string",false,"module name"),p!("limit","number",false,"max symbols")] },
    Tool { name: "list_maps", desc: "The lab's guest memory map (proc/self/maps shape).", params: &[] },
    Tool { name: "read_mem", desc: "Read guest memory in the lab; returns hex.", params: &[p!("address","string",true,"guest address (hex or decimal)"),p!("len","number",true,"bytes, max 1 MiB")] },
    Tool { name: "write_mem", desc: "Write guest memory in the lab, including into read-only code (copy-on-write + invalidate).", params: &[p!("address","string",true,"guest address"),p!("hex","string",true,"bytes as hex")] },
    Tool { name: "dump_module", desc: "Dump a loaded module out of guest memory (post-relocation / post-unpack). Optionally to a file.", params: &[p!("name","string",false,"module name"),p!("out_path","string",false,"write the dump here")] },
    Tool { name: "call_function", desc: "Call a guest function directly with crafted arguments (X0..X7). Returns the result and any events.", params: &[p!("symbol","string",false,"function symbol"),p!("address","string",false,"function address"),p!("args","array",false,"up to 8 integer/hex-string args")] },
    Tool { name: "set_breakpoint", desc: "Set a breakpoint (for run_until_stop/resume).", params: &[p!("symbol","string",false,"symbol"),p!("address","string",false,"address")] },
    Tool { name: "run_until_stop", desc: "Run a function until a breakpoint or return (interactive stepping).", params: &[p!("symbol","string",false,"entry symbol"),p!("address","string",false,"entry address"),p!("args","array",false,"args")] },
    Tool { name: "resume", desc: "Resume from a breakpoint stop.", params: &[] },
    Tool { name: "get_registers", desc: "The lab thread's registers (X0..X30, SP, PC, NZCV, TPIDR_EL0).", params: &[] },
    Tool { name: "backtrace", desc: "Walk the frame-pointer chain from the current stop.", params: &[] },
    Tool { name: "intercept", desc: "Intercept a function: record entry/exit args, or replace its return value so the body never runs.", params: &[p!("symbol","string",false,"symbol"),p!("address","string",false,"address"),p!("record_entry","boolean",false,"record entry (default true)"),p!("record_exit","boolean",false,"record exit (default true)"),p!("replace_return","string",false,"if set, force this return value")] },
    Tool { name: "trace_syscalls", desc: "Record every guest SVC during the next call.", params: &[p!("on","boolean",false,"on/off (default on)")] },
    Tool { name: "alloc_data", desc: "Place bytes (or zeroed space) in a writable guest region; returns the address (for crafted inputs).", params: &[p!("hex","string",false,"bytes as hex"),p!("len","number",false,"zeroed length")] },
    Tool { name: "load_code", desc: "Place A64 machine-code words in an executable guest region; returns the entry address.", params: &[p!("words","array",true,"32-bit A64 encodings")] },
];

// ---- small helpers ----------------------------------------------------------------------------

/// Wrap a JSON value as an MCP tool result (`{content:[{type:text,text:<json>}]}`).
fn text_result(value: Json) -> Json {
    json::obj([
        ("content", Json::Array(vec![json::obj([("type", json::s("text")), ("text", json::s(value.to_string()))])])),
        ("isError", Json::Bool(false)),
    ])
}

fn hex(v: u64) -> Json {
    json::s(format!("{v:#x}"))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let t = t.strip_prefix("0x").unwrap_or(&t);
    if t.len() % 2 != 0 {
        return Err("hex must have an even number of digits".into());
    }
    (0..t.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&t[i..i + 2], 16).map_err(|_| format!("bad hex at {i}")))
        .collect()
}

fn events_json(events: &[omni_debug::TraceEvent]) -> Json {
    use omni_debug::TraceKind::*;
    Json::Array(
        events
            .iter()
            .map(|e| {
                let kind = match e.kind {
                    Enter => "enter",
                    Exit => "exit",
                    Syscall => "syscall",
                    Replaced => "replaced",
                };
                let regs: Vec<Json> = e.regs.iter().map(|&v| hex(v)).collect();
                json::obj([
                    ("kind", json::s(kind)),
                    ("address", hex(e.address as u64)),
                    ("regs", Json::Array(regs)),
                    ("syscall_nr", Json::Num(e.syscall_nr as f64)),
                ])
            })
            .collect(),
    )
}

fn stop_json(stop: &Stop) -> Json {
    match stop {
        Stop::Returned(out) => json::obj([
            ("stopped", json::s("returned")),
            ("ret", hex(out.ret)),
            ("instructions", Json::Num(out.instructions as f64)),
            ("events", events_json(&out.events)),
        ]),
        Stop::Breakpoint { address, events } => json::obj([
            ("stopped", json::s("breakpoint")),
            ("address", hex(*address as u64)),
            ("events", events_json(events)),
        ]),
    }
}

/// Read width and height out of a PNG's IHDR, if the bytes are a PNG.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        assert_eq!(to_hex(&[0x00, 0xff, 0x10]), "00ff10");
        assert_eq!(from_hex("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert_eq!(from_hex("0xDEAD").unwrap(), vec![0xde, 0xad]);
        assert!(from_hex("abc").is_err());
    }

    #[test]
    fn png_dimensions_of_a_minimal_header() {
        let mut b = Vec::new();
        b.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        b.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&320u32.to_be_bytes());
        b.extend_from_slice(&240u32.to_be_bytes());
        assert_eq!(png_dimensions(&b), Some((320, 240)));
        assert_eq!(png_dimensions(b"not a png"), None);
    }

    #[test]
    fn tool_list_is_well_formed_and_covers_both_families() {
        let list = Server::tool_list();
        let tools = list.get("tools").unwrap().as_array().unwrap();
        assert!(tools.len() >= 20, "all tools listed");
        let names: Vec<&str> = tools.iter().filter_map(|t| t.get("name").and_then(Json::as_str)).collect();
        for expected in ["start_instance", "screenshot", "lab_load", "call_function", "dump_module", "read_mem", "write_mem", "resolve_symbol", "get_registers", "backtrace"] {
            assert!(names.contains(&expected), "tool {expected} must be listed");
        }
        // Every tool has an object inputSchema.
        for t in tools {
            assert_eq!(t.get("inputSchema").unwrap().get("type").unwrap().as_str(), Some("object"));
        }
    }
}
