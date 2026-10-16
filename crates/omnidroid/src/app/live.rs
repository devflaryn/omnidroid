//! **The app's MCP tools**: the pool, for coding agents -- any APK launched (as many instances as
//! asked, on the warm devices the rules pick), listed, waited for, looked at, typed into and
//! closed; the devices listed, warmed, stopped and given shell commands. The lab's tools (a `.so`
//! loaded, called, hooked, dumped) stay `omni-mcp`'s, beside these.
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_mcp::json::{self, Json};
use omni_mcp::mcp::RpcError;
use omni_pool::{InstanceState, Pool};

pub struct PoolLive {
    pub pool: Arc<Pool>,
}

/// A tool's description: name, what it does, its parameters as (name, type, required, what).
pub type ToolDef = (&'static str, &'static str, &'static [(&'static str, &'static str, bool, &'static str)]);

const TOOLS: &[ToolDef] = &[
    ("start_instance", "Launch any APK (a host path) as `count` instances (default 1). Answers their ids at once; each runs in a window of its own on a warm Android: the same APK file again shares an Android (one Android user per extra instance), another package goes into a warm Android beside the others, another build of a package running elsewhere gets an Android of its own. `wait_instance` waits until one runs.", &[("apk", "string", true, "host path of the APK"), ("count", "number", false, "how many instances (1-64, default 1)"), ("options", "string", false, "the installed plugins' options, as on omnidroid's command line: e.g. `--cookie <file or name login saved> --place <id>` (roblox plugin) signs each instance in as that account and joins the place")]),
    ("wait_instance", "Wait until an instance runs (its app's Activity displayed) or ends, at most `timeout` seconds (default 300). Answers its state.", &[("instance_id", "number", true, "the id start_instance gave"), ("timeout", "number", false, "seconds (default 300)")]),
    ("list_instances", "Every instance: id, package, its Android (device), user, display, state (waiting, installing, starting, running, closing, ended: why), screenshot path.", &[]),
    ("stop_instance", "Close an instance: its app stopped, its window gone. Its Android stays warm.", &[("instance_id", "number", true, "the instance")]),
    ("screenshot", "The instance's display as a PNG, written now: path, width, height.", &[("instance_id", "number", true, "the instance")]),
    ("input", "Input to an instance's display: action tap (x,y), swipe (x,y,x2,y2,ms), text (text), key (key: a KEYCODE_ name or number).", &[("instance_id", "number", true, "the instance"), ("action", "string", true, "tap | swipe | text | key"), ("x", "number", false, "x"), ("y", "number", false, "y"), ("x2", "number", false, "swipe end x"), ("y2", "number", false, "swipe end y"), ("ms", "number", false, "swipe duration"), ("text", "string", false, "text to type"), ("key", "string", false, "e.g. KEYCODE_BACK")]),
    ("shell", "A shell command on an Android (as `adb shell`): exit status and output. `device` defaults to the first ready one; `uid` 0 runs as root.", &[("command", "string", true, "a /system/bin/sh command line"), ("device", "number", false, "the Android's id (list_devices)"), ("uid", "number", false, "0 root, 1000 system, default 2000 shell")]),
    ("list_devices", "The warm Androids: id, state (booting, ready, closing), how long up and idle, builds installed, instances.", &[]),
    ("device_status", "The same as list_devices.", &[]),
    ("warm_device", "Boot one more Android now (so later launches start at once).", &[]),
    ("stop_device", "Stop an Android and the instances on it. Stopping the last one leaves none: no other is warmed (nor when the app opens next) until a launch or `warm_device`.", &[("device", "number", true, "the Android's id")]),
    ("settings", "The app's settings; with `key` and `value`, change one (idle_close_minutes, keep_warm, warm_on_start, stop_on_exit, max_devices, instance_size, gpu, device_ram_mb).", &[("key", "string", false, "a setting"), ("value", "string", false, "its new value")]),
    ("pool_log", "The app's log (launches, installs, boots, ends), newest last.", &[("lines", "number", false, "how many (default 60)")]),
];

fn arg_u64(args: &Json, name: &str) -> Option<u64> {
    args.get(name).and_then(|v| v.as_f64().map(|f| f as u64).or_else(|| v.as_str().and_then(|s| s.trim_start_matches(['#', 'D']).parse().ok())))
}

fn need_u64(args: &Json, name: &str) -> Result<u64, RpcError> {
    arg_u64(args, name).ok_or_else(|| RpcError::params(format!("need `{name}`")))
}

fn state_name(s: &InstanceState) -> String {
    match s {
        InstanceState::Waiting => "waiting".into(),
        InstanceState::Installing => "installing".into(),
        InstanceState::Starting => "starting".into(),
        InstanceState::Running => "running".into(),
        InstanceState::Closing => "closing".into(),
        InstanceState::Ended(why) => format!("ended: {why}"),
    }
}

/// A PNG's width and height (its IHDR).
fn png_size(path: &Path) -> Option<(u32, u32)> {
    let mut head = [0u8; 24];
    let mut f = std::fs::File::open(path).ok()?;
    std::io::Read::read_exact(&mut f, &mut head).ok()?;
    Some((u32::from_be_bytes(head[16..20].try_into().ok()?), u32::from_be_bytes(head[20..24].try_into().ok()?)))
}

impl PoolLive {
    fn instances(&self) -> Json {
        let (_, instances) = self.pool.snapshot();
        Json::Array(
            instances
                .iter()
                .map(|i| {
                    json::obj([
                        ("instance_id", Json::Num(f64::from(i.id))),
                        ("package", json::s(i.package.clone())),
                        ("apk", json::s(i.apk.display().to_string())),
                        ("sha256", json::s(i.sha256.clone())),
                        ("device", Json::Num(f64::from(i.device))),
                        ("user", i.user.map_or(Json::Null, |u| Json::Num(f64::from(u)))),
                        ("display", i.display.map_or(Json::Null, |d| Json::Num(f64::from(d)))),
                        ("state", json::s(state_name(&i.state))),
                        ("up_s", Json::Num(i.up_for.as_secs() as f64)),
                        ("screenshot", i.screenshot.as_ref().map_or(Json::Null, |p| json::s(p.display().to_string()))),
                    ])
                })
                .collect(),
        )
    }

    fn devices(&self) -> Json {
        let (devices, _) = self.pool.snapshot();
        Json::Array(
            devices
                .iter()
                .map(|d| {
                    json::obj([
                        ("device", Json::Num(f64::from(d.id))),
                        ("state", json::s(format!("{:?}", d.state).to_lowercase())),
                        ("up_s", Json::Num(d.up_for.as_secs() as f64)),
                        ("idle_s", d.idle_for.map_or(Json::Null, |t| Json::Num(t.as_secs() as f64))),
                        ("instances", Json::Num(d.instances as f64)),
                        ("installed", Json::Array(d.installed.iter().map(|(p, s)| json::s(format!("{p} ({})", &s[..12.min(s.len())]))).collect())),
                        ("dir", json::s(d.dir.display().to_string())),
                        ("log", json::s(d.dir.with_extension("log").display().to_string())),
                    ])
                })
                .collect(),
        )
    }

    fn instance_display(&self, id: u32) -> Result<(u32, i32), RpcError> {
        let (_, instances) = self.pool.snapshot();
        let i = instances.iter().find(|i| i.id == id).ok_or_else(|| RpcError::params(format!("no instance #{id}")))?;
        match (&i.state, i.display) {
            (InstanceState::Running, Some(d)) => Ok((i.device, d)),
            (s, _) => Err(RpcError::server(format!("#{id} is {} (not running)", state_name(s)))),
        }
    }

    fn run(&mut self, name: &str, args: &Json) -> Result<Json, RpcError> {
        let pool = &self.pool;
        match name {
            "start_instance" | "launch_app" => {
                let apk = args.get("apk").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `apk` (a host path)"))?;
                let count = arg_u64(args, "count").unwrap_or(1).clamp(1, 64) as u32;
                let options = args.get("options").and_then(Json::as_str).map(super::command::words).unwrap_or_default();
                let extras = super::plugged::extras(Path::new(apk), &options).map_err(RpcError::params)?;
                let ids = pool.launch_with(Path::new(apk), count, extras).map_err(RpcError::server)?;
                Ok(json::obj([
                    ("instances", Json::Array(ids.iter().map(|i| Json::Num(f64::from(*i))).collect())),
                    ("note", json::s("launching: wait_instance {instance_id} waits until it runs; list_instances shows every state")),
                ]))
            }
            "wait_instance" => {
                let id = need_u64(args, "instance_id")? as u32;
                let limit = Duration::from_secs(arg_u64(args, "timeout").unwrap_or(300).min(3600));
                let t = Instant::now();
                loop {
                    let (_, instances) = pool.snapshot();
                    let i = instances.iter().find(|i| i.id == id).ok_or_else(|| RpcError::params(format!("no instance #{id}")))?;
                    let done = matches!(i.state, InstanceState::Running | InstanceState::Ended(_));
                    if done || t.elapsed() >= limit {
                        return Ok(json::obj([
                            ("instance_id", Json::Num(f64::from(id))),
                            ("state", json::s(state_name(&i.state))),
                            ("waited_s", Json::Num((t.elapsed().as_millis() as f64 / 100.0).round() / 10.0)),
                            ("screenshot", i.screenshot.as_ref().map_or(Json::Null, |p| json::s(p.display().to_string()))),
                        ]));
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
            "list_instances" => Ok(json::obj([("instances", self.instances())])),
            "stop_instance" => {
                let id = need_u64(args, "instance_id")? as u32;
                pool.close(id).map_err(RpcError::params)?;
                Ok(json::obj([("instance_id", Json::Num(f64::from(id))), ("closing", Json::Bool(true))]))
            }
            "screenshot" => {
                let id = need_u64(args, "instance_id")? as u32;
                let (_, instances) = pool.snapshot();
                let i = instances.iter().find(|i| i.id == id).ok_or_else(|| RpcError::params(format!("no instance #{id}")))?;
                let path = i.screenshot.clone().ok_or_else(|| RpcError::server(format!("#{id} has no display yet ({})", state_name(&i.state))))?;
                // The device writes the display's frame now (`omni_linux::displays::keep_screenshot`)
                // and removes the request once the PNG is current.
                let want = omni_linux::displays::screenshot_want(&path);
                if std::fs::write(&want, b"").is_ok() {
                    let t = Instant::now();
                    while want.exists() && t.elapsed() < Duration::from_secs(3) {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
                let (w, h) = png_size(&path).ok_or_else(|| RpcError::server(format!("no frame yet at {}", path.display())))?;
                Ok(json::obj([("path", json::s(path.display().to_string())), ("width", Json::Num(f64::from(w))), ("height", Json::Num(f64::from(h)))]))
            }
            "input" => {
                let id = need_u64(args, "instance_id")? as u32;
                let (device, display) = self.instance_display(id)?;
                let n = |k: &str| need_u64(args, k).map(|v| v.to_string());
                let command = match args.get("action").and_then(Json::as_str).unwrap_or("") {
                    "tap" => format!("input -d {display} tap {} {}", n("x")?, n("y")?),
                    "swipe" => format!("input -d {display} swipe {} {} {} {} {}", n("x")?, n("y")?, n("x2")?, n("y2")?, arg_u64(args, "ms").unwrap_or(300)),
                    "text" => {
                        let text = args.get("text").and_then(Json::as_str).ok_or_else(|| RpcError::params("text needs `text`"))?;
                        // Spaces are %s to `input text`; the rest single-quoted for the shell.
                        format!("input -d {display} text '{}'", text.replace(' ', "%s").replace('\'', "'\\''"))
                    }
                    "key" => {
                        let key = args.get("key").and_then(Json::as_str).ok_or_else(|| RpcError::params("key needs `key`"))?;
                        if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                            return Err(RpcError::params(format!("not a key: {key}")));
                        }
                        format!("input -d {display} keyevent {key}")
                    }
                    other => return Err(RpcError::params(format!("action is tap, swipe, text or key, not `{other}`"))),
                };
                let dev = pool.device(device).ok_or_else(|| RpcError::server(format!("its Android D{device} is gone")))?;
                let (code, out) = dev.shell(&command, Duration::from_secs(60)).map_err(RpcError::server)?;
                Ok(json::obj([("exit", Json::Num(code as f64)), ("output", json::s(out))]))
            }
            "shell" => {
                let command = args.get("command").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `command`"))?;
                let (devices, _) = pool.snapshot();
                let id = match arg_u64(args, "device") {
                    Some(d) => d as u32,
                    None => devices.iter().find(|d| d.state == omni_pool::DeviceState::Ready).map(|d| d.id).ok_or_else(|| RpcError::server("no Android is ready (warm_device boots one)"))?,
                };
                let dev = pool.device(id).ok_or_else(|| RpcError::params(format!("no Android D{id}")))?;
                let uid = arg_u64(args, "uid").map(|u| u as u32);
                let (code, out) = dev.shell_as(command, uid, Duration::from_secs(arg_u64(args, "timeout").unwrap_or(120))).map_err(RpcError::server)?;
                Ok(json::obj([("device", Json::Num(f64::from(id))), ("exit", Json::Num(code as f64)), ("output", json::s(out))]))
            }
            "list_devices" | "device_status" => Ok(json::obj([("devices", self.devices())])),
            "warm_device" => Ok(json::obj([("device", Json::Num(f64::from(pool.warm()))), ("note", json::s("booting; list_devices shows when it is ready"))])),
            "stop_device" => {
                let id = need_u64(args, "device")? as u32;
                let pool = Arc::clone(pool);
                std::thread::spawn(move || pool.kill(id));
                Ok(json::obj([("device", Json::Num(f64::from(id))), ("stopping", Json::Bool(true))]))
            }
            "settings" => {
                if let (Some(key), Some(value)) = (args.get("key").and_then(Json::as_str), args.get("value").and_then(Json::as_str)) {
                    if key.starts_with("mcp_") {
                        return Err(RpcError::params("the MCP server's own settings are changed in the app"));
                    }
                    pool.set(key, value).map_err(RpcError::params)?;
                }
                let c = pool.config();
                Ok(Json::Object(omni_pool::config::KEYS.iter().map(|(k, _)| ((*k).to_string(), json::s(c.get(k).unwrap_or_default()))).collect()))
            }
            "pool_log" => {
                let n = arg_u64(args, "lines").unwrap_or(60) as usize;
                let log = pool.log();
                Ok(json::obj([("lines", Json::Array(log[log.len().saturating_sub(n)..].iter().map(|l| json::s(l.clone())).collect()))]))
            }
            other => Err(RpcError::params(format!("no tool {other}"))),
        }
    }
}

impl omni_mcp::Live for PoolLive {
    fn tools(&self) -> Vec<Json> {
        TOOLS
            .iter()
            .chain(super::web::TOOLS)
            .map(|(name, desc, params)| {
                let props: std::collections::BTreeMap<String, Json> = params.iter().map(|(n, t, _, d)| ((*n).to_string(), json::obj([("type", json::s(if *t == "number" { "number" } else { t })), ("description", json::s(*d))]))).collect();
                let required: Vec<Json> = params.iter().filter(|p| p.2).map(|p| json::s(p.0)).collect();
                json::obj([
                    ("name", json::s(*name)),
                    ("description", json::s(*desc)),
                    ("inputSchema", json::obj([("type", json::s("object")), ("properties", Json::Object(props)), ("required", Json::Array(required))])),
                ])
            })
            .collect()
    }

    fn call(&mut self, name: &str, arguments: &Json) -> Option<Result<Json, RpcError>> {
        if let Some(answer) = super::web::call(&self.pool, name, arguments) {
            return Some(answer);
        }
        let known = TOOLS.iter().any(|t| t.0 == name) || name == "launch_app";
        known.then(|| self.run(name, arguments))
    }
}
