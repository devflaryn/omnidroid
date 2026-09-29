//! The MCP server: configuration, the live-instance registry, the lab debug session, and the
//! dispatch of every tool.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Child;

use omni_debug::{HookAction, Session, Stop};

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
        }
    }
}

/// One booted omnidroid instance.
struct Instance {
    id: String,
    child: Child,
    screenshot: PathBuf,
    apk: Option<PathBuf>,
    place: Option<String>,
    started: std::time::Instant,
}

/// The MCP server.
pub struct Server {
    config: Config,
    instances: BTreeMap<String, Instance>,
    next_instance: u64,
    lab: Option<Session>,
    lab_source: Option<String>,
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
        }
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

        let id = format!("inst-{}", self.next_instance);
        self.next_instance += 1;
        let screenshot = std::env::temp_dir().join(format!("omni-mcp-{id}.png"));
        let _ = std::fs::remove_file(&screenshot);

        let mut cmd = std::process::Command::new(&self.config.omnidroid_bin);
        cmd.arg("aosp").arg("--apk").arg(&apk).arg("--gpu").arg(&gpu);
        if let Some(c) = &cookie {
            cmd.arg("--cookie").arg(c);
        }
        if let Some(p) = &place {
            cmd.arg("--place").arg(p);
        }
        if let Some(sz) = &size {
            cmd.arg("--size").arg(sz);
        }
        if let Some(m) = minutes {
            cmd.arg("--minutes").arg(m.to_string());
        }
        // The screenshot path we can read (r_roblox honours a preset OMNI_SCREENSHOT).
        cmd.env("OMNI_SCREENSHOT", &screenshot);
        if let Some(ram) = self.config.device_ram_mb {
            cmd.env("OMNI_DEVICE_RAM_MB", ram.to_string());
        }
        if let Some(repo) = &self.config.repo_dir {
            cmd.current_dir(repo);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let child = cmd.spawn().map_err(|e| {
            RpcError::server(format!("could not start {}: {e}", self.config.omnidroid_bin.display()))
        })?;
        let pid = child.id();
        let instance = Instance {
            id: id.clone(),
            child,
            screenshot: screenshot.clone(),
            apk: Some(apk),
            place: place.clone(),
            started: std::time::Instant::now(),
        };
        self.instances.insert(id.clone(), instance);

        Ok(json::obj([
            ("instance_id", json::s(id)),
            ("pid", Json::Num(pid as f64)),
            ("screenshot_path", json::s(screenshot.to_string_lossy().into_owned())),
            ("note", json::s("booting; poll `screenshot` for a frame (a boot takes minutes)")),
        ]))
    }

    fn stop_instance(&mut self, args: &Json) -> Result<Json, RpcError> {
        let id = args.get("instance_id").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `instance_id`"))?;
        let mut inst = self.instances.remove(id).ok_or_else(|| RpcError::params(format!("no instance {id}")))?;
        let _ = inst.child.kill();
        let _ = inst.child.wait();
        Ok(json::obj([("instance_id", json::s(id)), ("stopped", Json::Bool(true))]))
    }

    fn list_instances(&mut self) -> Json {
        let mut out = Vec::new();
        for inst in self.instances.values_mut() {
            let running = matches!(inst.child.try_wait(), Ok(None));
            out.push(json::obj([
                ("instance_id", json::s(inst.id.clone())),
                ("pid", Json::Num(inst.child.id() as f64)),
                ("running", Json::Bool(running)),
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
        // On the real-AOSP path the launcher installs the APK as part of a boot, so this records the
        // APK as the default for the next start/launch rather than installing into a live device.
        self.config.apk = Some(apk.clone());
        Ok(json::obj([
            ("apk", json::s(apk.to_string_lossy().into_owned())),
            ("note", json::s("recorded as the default APK; `start_instance`/`launch_app` will install and boot it")),
        ]))
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
            "notifications/initialized" | "initialized" => Ok(Json::Null),
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
        // Do not leave booted instances running when the server exits.
        for inst in self.instances.values_mut() {
            let _ = inst.child.kill();
            let _ = inst.child.wait();
        }
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
    Tool { name: "start_instance", desc: "Boot a real omnidroid instance for an APK (installs, launches, optionally signs in and joins a place). Long-running.", params: &[
        p!("apk","string",false,"APK path (else the configured default)"),
        p!("cookie","string",false,"cookie file path or saved account name"),
        p!("place","string",false,"place id to join (e.g. 8737899170)"),
        p!("gpu","string",false,"vulkan | gl | auto"),
        p!("size","string",false,"window size WxH"),
        p!("minutes","number",false,"minutes to run"),
    ]},
    Tool { name: "stop_instance", desc: "Stop a running instance.", params: &[p!("instance_id","string",true,"the id from start_instance")] },
    Tool { name: "list_instances", desc: "List instances this server has started and whether each is still running.", params: &[] },
    Tool { name: "install_apk", desc: "Record an APK as the default to install and boot on the next start_instance/launch_app.", params: &[p!("apk","string",true,"APK path")] },
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
