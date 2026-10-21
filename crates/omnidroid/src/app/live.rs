//! **The app's MCP tools**: the pool, for coding agents -- any APK launched (as many instances as
//! asked, on the warm devices the rules pick), listed, waited for, looked at, typed into,
//! restarted (now, or whenever it crashes) and closed; the devices listed, warmed, stopped and
//! given shell commands. The lab's tools (a `.so` loaded, called, hooked, dumped) stay
//! `omni-mcp`'s, beside these.
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_mcp::json::{self, Json};
use omni_mcp::mcp::RpcError;
use omni_pool::{InstanceInfo, InstanceState, Pool};

pub struct PoolLive {
    pub pool: Arc<Pool>,
}

/// A tool's description: name, what it does, its parameters as (name, type, required, what).
pub type ToolDef = (&'static str, &'static str, &'static [(&'static str, &'static str, bool, &'static str)]);

const TOOLS: &[ToolDef] = &[
    ("start_instance", "Launch any APK (a host path) as `count` instances (default 1). Answers their ids at once; each runs in a window of its own on a warm Android: the same APK file again shares an Android (one Android user per extra instance), another package goes into a warm Android beside the others, another build of a package running elsewhere gets an Android of its own. `wait_instance` waits until one runs. `auto_restart` sets each new instance's own auto restart (else the pool's setting holds: `auto_restart` tool).", &[("apk", "string", true, "host path of the APK"), ("count", "number", false, "how many instances (1-64, default 1)"), ("options", "string", false, "the installed plugins' options, as on omnidroid's command line: e.g. `--cookie <file or name login saved> --place <id>` (roblox plugin) signs each instance in as that account and joins the place"), ("auto_restart", "boolean", false, "restart each new instance's app when it crashes (its own setting)")]),
    ("wait_instance", "Wait until an instance runs (its app's Activity displayed) or ends, at most `timeout` seconds (default 300); one restarting is waited through until it runs again (or ends). Answers its state, auto_restart, restarts and, while restarting, restart_in (seconds); an ended or restarting one's `details` too (what its Android's log says happened, as the card its window shows), and once restarted, `last_end` (the same, of the end it was restarted after). An app closed on purpose (stop_instance, its window closed, force-stopped, quit by the person) ends with its window closed and no card.", &[("instance_id", "number", true, "the id start_instance gave"), ("timeout", "number", false, "seconds (default 300)")]),
    ("list_instances", "Every instance: id, package, its Android (device), user, display, state (waiting, installing, starting, running, restarting, closing, ended: why), auto_restart (its own setting, else the pool's), restarts (how many so far), restart_in (seconds, while restarting), screenshot path; an ended or restarting one's `details` too -- when its app crashed, was killed, or did not start, what its Android's log says happened (exception and frames, signal and library, ANR...), the card its window shows until it is closed or restarted -- and a restarted one's `last_end` (the same, of the end it was restarted after). An app closed on purpose (stop_instance, its window closed, force-stopped, quit by the person) ends with its window closed and no card.", &[]),
    ("stop_instance", "Close an instance: its app stopped, its window gone (no card, no auto restart). Its Android stays warm.", &[("instance_id", "number", true, "the instance")]),
    ("restart_instance", "Restart an instance's app on its window: its app stopped (a running one) and started again after `delay` seconds (default 5) while its window's card counts down (\"Restarting in 5 seconds\", \"Click here to restart now\"); a crashed one's card shows the countdown under its crash log. Same instance id. wait_instance waits for it to run again. Answers its state.", &[("instance_id", "number", true, "the instance"), ("delay", "number", false, "seconds before its app starts again (default 5; 0 now)")]),
    ("auto_restart", "Restart an app automatically when it crashes or ends on its own (its card says \"Automatically restarting in 5 seconds\"): for one instance, or without instance_id for every instance without its own setting (saved). After 3 ends in a row within a minute of starting, it stops restarting it. An app closed on purpose (stop_instance, its window closed, force-stopped, quit by the person) is not restarted: its window closes. Answers the setting now.", &[("on", "boolean", true, "true: restart it when it crashes; false: leave it ended, its card shown"), ("instance_id", "number", false, "the instance (without: the pool's setting)")]),
    ("screenshot", "The instance's display as a PNG, written now: path, width, height.", &[("instance_id", "number", true, "the instance")]),
    ("input", "Input to an instance's display: action tap (x,y), swipe (x,y,x2,y2,ms), text (text), key (key: a KEYCODE_ name or number).", &[("instance_id", "number", true, "the instance"), ("action", "string", true, "tap | swipe | text | key"), ("x", "number", false, "x"), ("y", "number", false, "y"), ("x2", "number", false, "swipe end x"), ("y2", "number", false, "swipe end y"), ("ms", "number", false, "swipe duration"), ("text", "string", false, "text to type"), ("key", "string", false, "e.g. KEYCODE_BACK")]),
    ("shell", "A shell command on an Android (as `adb shell`): exit status and output. `device` defaults to the first ready one; `uid` 0 runs as root.", &[("command", "string", true, "a /system/bin/sh command line"), ("device", "number", false, "the Android's id (list_devices)"), ("uid", "number", false, "0 root, 1000 system, default 2000 shell")]),
    ("list_devices", "The warm Androids: id, state (booting, ready, closing), how long up and idle, builds installed, instances.", &[]),
    ("device_status", "The same as list_devices.", &[]),
    ("warm_device", "Boot one more Android now (so later launches start at once).", &[]),
    ("stop_device", "Stop an Android and the instances on it. Stopping the last one leaves none: no other is warmed (nor when the app opens next) until a launch or `warm_device`.", &[("device", "number", true, "the Android's id")]),
    ("settings", "The app's settings; with `key` and `value`, change one (idle_close_minutes, keep_warm, warm_on_start, stop_on_exit, max_devices, instance_size, gpu, device_ram_mb, auto_restart).", &[("key", "string", false, "a setting"), ("value", "string", false, "its new value")]),
    ("pool_log", "The app's log (launches, installs, boots, ends), newest last.", &[("lines", "number", false, "how many (default 60)")]),
];

fn arg_u64(args: &Json, name: &str) -> Option<u64> {
    args.get(name).and_then(|v| v.as_f64().map(|f| f as u64).or_else(|| v.as_str().and_then(|s| s.trim_start_matches(['#', 'D']).parse().ok())))
}

fn need_u64(args: &Json, name: &str) -> Result<u64, RpcError> {
    arg_u64(args, name).ok_or_else(|| RpcError::params(format!("need `{name}`")))
}

/// A boolean argument: `true`/`false`, or as the settings take one (`on`, `off`, ...).
fn arg_bool(args: &Json, name: &str) -> Option<bool> {
    let v = args.get(name)?;
    v.as_bool().or_else(|| match v.as_str()? {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    })
}

/// Seconds as given (a number, or text holding one), not negative.
fn arg_secs(args: &Json, name: &str) -> Option<Duration> {
    let v = args.get(name)?;
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()).filter(|s| s.is_finite() && *s >= 0.0).map(|s| Duration::from_secs_f64(s.min(3600.0)))
}

fn state_name(s: &InstanceState) -> String {
    match s {
        InstanceState::Waiting => "waiting".into(),
        InstanceState::Installing => "installing".into(),
        InstanceState::Starting => "starting".into(),
        InstanceState::Running => "running".into(),
        InstanceState::Closing => "closing".into(),
        InstanceState::Restarting => "restarting".into(),
        InstanceState::Ended(why) => format!("ended: {why}"),
    }
}

/// What its device's log says happened to its app (`omni_pool::crash`): an ended or restarting
/// instance's `details`, as its card shows them -- an auto restart's crash too, which went
/// unsaid while only an ended one's were answered --, and one's `last_end` once it was started
/// again: `wait_instance` waits through a restart, and the card counts down 5 s. None for one
/// nothing ended.
fn details(i: &InstanceInfo) -> Option<(&'static str, Json)> {
    let key = match i.state {
        InstanceState::Ended(_) => "details",
        _ if i.details.is_empty() => return None,
        InstanceState::Restarting => "details",
        _ => "last_end",
    };
    Some((key, Json::Array(i.details.iter().cloned().map(json::s).collect())))
}

/// An instance's restarts: its auto restart setting, how many so far, and -- while restarting --
/// the seconds its card still counts down.
fn restarts(i: &InstanceInfo) -> impl Iterator<Item = (&'static str, Json)> {
    [("auto_restart", Json::Bool(i.auto_restart)), ("restarts", Json::Num(f64::from(i.restarts)))]
        .into_iter()
        .chain(i.restart_in.filter(|_| i.state == InstanceState::Restarting).map(|t| ("restart_in", Json::Num((t.as_secs_f64() * 10.0).round() / 10.0))))
}

/// One instance as `list_instances` answers it.
fn instance_json(i: &InstanceInfo) -> Json {
    json::obj(
        [
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
        ]
        .into_iter()
        .chain(restarts(i))
        .chain(details(i)),
    )
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
        Json::Array(instances.iter().map(instance_json).collect())
    }

    /// One instance now, as `list_instances` answers it.
    fn instance(&self, id: u32) -> Result<Json, RpcError> {
        let (_, instances) = self.pool.snapshot();
        instances.iter().find(|i| i.id == id).map(instance_json).ok_or_else(|| RpcError::params(format!("no instance #{id}")))
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
                if let Some(on) = arg_bool(args, "auto_restart") {
                    for id in &ids {
                        let _ = pool.set_auto_restart(Some(*id), on);
                    }
                }
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
                    // A restarting one is neither: its app runs again in a moment.
                    let done = matches!(i.state, InstanceState::Running | InstanceState::Ended(_));
                    if done || t.elapsed() >= limit {
                        return Ok(json::obj(
                            [
                                ("instance_id", Json::Num(f64::from(id))),
                                ("state", json::s(state_name(&i.state))),
                                ("waited_s", Json::Num((t.elapsed().as_millis() as f64 / 100.0).round() / 10.0)),
                                ("screenshot", i.screenshot.as_ref().map_or(Json::Null, |p| json::s(p.display().to_string()))),
                            ]
                            .into_iter()
                            .chain(restarts(i))
                            .chain(details(i)),
                        ));
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
            "restart_instance" => {
                let id = need_u64(args, "instance_id")? as u32;
                pool.restart(id, arg_secs(args, "delay").unwrap_or(omni_pool::RESTART_DELAY)).map_err(RpcError::server)?;
                self.instance(id)
            }
            "auto_restart" => {
                let on = arg_bool(args, "on").ok_or_else(|| RpcError::params("need `on` (true or false)"))?;
                let id = arg_u64(args, "instance_id").map(|i| i as u32);
                pool.set_auto_restart(id, on).map_err(RpcError::params)?;
                match id {
                    Some(id) => self.instance(id),
                    None => {
                        let (_, instances) = pool.snapshot();
                        let live = instances.iter().filter(|i| !matches!(i.state, InstanceState::Ended(_)));
                        Ok(json::obj([
                            ("auto_restart", Json::Bool(pool.config().auto_restart)),
                            ("instances", Json::Array(live.map(|i| json::obj([("instance_id", Json::Num(f64::from(i.id))), ("auto_restart", Json::Bool(i.auto_restart))])).collect())),
                        ]))
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use omni_mcp::Live;

    #[test]
    fn restart_and_auto_restart_are_tools() {
        let config = omni_pool::Config { warm_on_start: false, ..Default::default() };
        // Adopting none: the person's own pool keeps its devices and windows.
        let pool = Pool::start_alone(config, None, std::path::PathBuf::from("omnidroid"));
        let mut live = PoolLive { pool: Arc::clone(&pool) };
        let tools = live.tools();
        let tool = |name: &str| tools.iter().find(|t| t.get("name").and_then(Json::as_str) == Some(name)).cloned().unwrap_or_else(|| panic!("no tool {name}"));
        let prop = |t: &Json, p: &str| t.get("inputSchema").and_then(|s| s.get("properties")).and_then(|s| s.get(p)).and_then(|p| p.get("type")).and_then(Json::as_str).map(str::to_string);
        assert_eq!(prop(&tool("restart_instance"), "delay").as_deref(), Some("number"));
        assert_eq!(prop(&tool("auto_restart"), "on").as_deref(), Some("boolean"));
        assert_eq!(prop(&tool("start_instance"), "auto_restart").as_deref(), Some("boolean"));
        let on = live.call("auto_restart", &json::obj([("on", Json::Bool(true))])).expect("a tool").expect("the pool's setting");
        assert_eq!(on.get("auto_restart").and_then(Json::as_bool), Some(true));
        assert!(pool.config().auto_restart, "the pool's setting, for every instance without its own");
        assert!(live.call("auto_restart", &json::obj([("on", json::s("off")), ("instance_id", Json::Num(9.0))])).expect("a tool").is_err(), "no instance #9");
        assert!(live.call("auto_restart", &Json::Null).expect("a tool").is_err(), "`on` is needed");
        assert!(live.call("restart_instance", &json::obj([("instance_id", Json::Num(9.0)), ("delay", Json::Num(0.0))])).expect("a tool").is_err(), "no instance #9");
        pool.shutdown(false);
    }

    /// A crash's details while its auto restart counts down, and after it, as `last_end`.
    #[test]
    fn a_crash_s_details_are_answered_while_it_restarts_and_after() {
        let info = |state, details: &[&str], restarts| InstanceInfo {
            id: 1,
            device: 1,
            package: "com.a".into(),
            sha256: "f".into(),
            apk: "/a.apk".into(),
            user: Some(10),
            display: Some(3),
            screenshot: None,
            state,
            up_for: Duration::ZERO,
            details: details.iter().map(|l| (*l).to_string()).collect(),
            auto_restart: true,
            restarts,
            restart_in: None,
        };
        let key = |i: &InstanceInfo| details(i).map(|(k, _)| k);
        assert_eq!(key(&info(InstanceState::Ended("closed".into()), &[], 0)), Some("details"), "an ended one's, always");
        assert_eq!(key(&info(InstanceState::Restarting, &["boom"], 0)), Some("details"), "the card's, while it counts down");
        assert_eq!(key(&info(InstanceState::Running, &["boom"], 1)), Some("last_end"), "started again");
        assert_eq!(key(&info(InstanceState::Restarting, &[], 0)), None, "a restart asked while it ran: nothing ended it");
        assert_eq!(key(&info(InstanceState::Running, &[], 0)), None);
    }
}
