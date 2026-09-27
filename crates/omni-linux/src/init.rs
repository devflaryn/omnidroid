//! init's services, read from the image's own `.rc` files: every `service` stanza (its program,
//! arguments, user, classes, `disabled`, `oneshot`, and the AIDL/HIDL interfaces it serves), and
//! what starts them -- `class_start` of a class, `ctl.start` and `ctl.interface_start` set by
//! another process (servicemanager starting a lazy service).
//!
//! Each service runs as its own guest process. What init also does at boot -- mounts, SELinux
//! policy, `on` triggers -- is not done here; the directories its `mkdir`s make are
//! (`crate::boot`).
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use crate::fd::Output;
use crate::process::{Process, SpawnConfig};
use crate::vfs::Sysroot;

#[derive(Debug, Clone, Default)]
pub struct Service {
    pub name: String,
    pub argv: Vec<String>,
    pub uid: u32,
    pub classes: Vec<String>,
    pub disabled: bool,
    pub oneshot: bool,
    /// `interface aidl <name>` and `interface <hidl@version::IFoo> <instance>`.
    pub interfaces: Vec<String>,
}

/// A user name as `android_filesystem_config.h` numbers it.
fn uid_of(user: &str) -> u32 {
    match user {
        "root" => 0,
        "system" => 1000,
        "radio" => 1001,
        "bluetooth" => 1002,
        "graphics" => 1003,
        "input" => 1004,
        "audio" => 1005,
        "camera" => 1006,
        "log" => 1007,
        "wifi" => 1010,
        "media" => 1013,
        "drm" => 1019,
        "gps" => 1021,
        "nfc" => 1027,
        "shell" => 2000,
        "cameraserver" => 1047,
        "keystore" => 1017,
        "statsd" => 1066,
        "hsm" => 1076,
        "nobody" => 9999,
        other => other.parse().unwrap_or(1000),
    }
}

/// The boot phases init triggers unconditionally, in order: a `start` in their `on` blocks runs.
const BOOT_PHASES: [&str; 11] =
    ["early-init", "init", "late-init", "early-fs", "fs", "post-fs", "late-fs", "post-fs-data", "zygote-start", "early-boot", "boot"];

/// Every `service` in the image's init scripts, by name.
#[must_use]
pub fn services(sysroot: &Sysroot) -> HashMap<String, Service> {
    parse(sysroot).0
}

/// A command of a boot phase's `on` block that init carries out here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Start(String),
    /// `exec_start`: start a service and wait for it to end.
    ExecStart(String),
    Export(String, String),
    /// `load_exports <file>`: its `export NAME VALUE` lines.
    LoadExports(String),
}

/// The services, and the commands of the boot-phase `on` blocks, in phase order.
fn parse(sysroot: &Sysroot) -> (HashMap<String, Service>, Vec<Command>) {
    // The ramdisk's global environment first (init.rc imports it), then init.rc.
    let mut scripts: Vec<Vec<u8>> = vec![b"/init.environ.rc".to_vec(), b"/system/etc/init/hw/init.rc".to_vec()];
    let mut dirs: Vec<Vec<u8>> =
        vec![b"/system/etc/init".to_vec(), b"/system_ext/etc/init".to_vec(), b"/product/etc/init".to_vec(), b"/vendor/etc/init".to_vec(), b"/odm/etc/init".to_vec()];
    for apex in sysroot.children(b"/apex") {
        let mut d = b"/apex/".to_vec();
        d.extend_from_slice(&apex);
        d.extend_from_slice(b"/etc");
        dirs.push(d);
    }
    for dir in dirs {
        for name in sysroot.children(&dir) {
            if name.ends_with(b".rc") {
                let mut path = dir.clone();
                path.push(b'/');
                path.extend_from_slice(&name);
                scripts.push(path);
            }
        }
    }
    let mut out = HashMap::new();
    let mut commands: Vec<(usize, Command)> = Vec::new();
    for script in scripts {
        let Some(text) = sysroot.read(&script) else { continue };
        let mut current: Option<Service> = None;
        // The boot phase of the `on` block being read, when it is one with no other condition.
        let mut phase: Option<usize> = None;
        for line in String::from_utf8_lossy(&text).lines() {
            let line = line.trim();
            let words: Vec<&str> = line.split_whitespace().collect();
            match words.first().copied() {
                Some("service") if words.len() >= 3 => {
                    if let Some(s) = current.take() {
                        out.insert(s.name.clone(), s);
                    }
                    current = Some(Service {
                        name: words[1].to_string(),
                        argv: words[2..].iter().map(|w| (*w).to_string()).collect(),
                        uid: 0,
                        classes: vec!["default".to_string()],
                        ..Service::default()
                    });
                }
                Some("on" | "import") => {
                    if let Some(s) = current.take() {
                        out.insert(s.name.clone(), s);
                    }
                    phase = (words.first() == Some(&"on") && words.len() == 2).then(|| BOOT_PHASES.iter().position(|p| *p == words[1])).flatten();
                }
                Some("start" | "exec_start" | "export" | "load_exports") if current.is_none() => {
                    let command = match (words[0], words.len()) {
                        ("start", 2) => Some(Command::Start(words[1].to_string())),
                        ("exec_start", 2) => Some(Command::ExecStart(words[1].to_string())),
                        ("export", 3) => Some(Command::Export(words[1].to_string(), words[2].to_string())),
                        ("load_exports", 2) => Some(Command::LoadExports(words[1].to_string())),
                        _ => None,
                    };
                    if let (Some(ph), Some(c)) = (phase, command) {
                        commands.push((ph, c));
                    }
                }
                Some(option) => {
                    let Some(s) = current.as_mut() else { continue };
                    match option {
                        "user" if words.len() >= 2 => s.uid = uid_of(words[1]),
                        "class" => s.classes = words[1..].iter().map(|w| (*w).to_string()).collect(),
                        "disabled" => s.disabled = true,
                        "oneshot" => s.oneshot = true,
                        "interface" if words.len() >= 3 => s.interfaces.push(words[1..].join(" ")),
                        _ => {}
                    }
                }
                None => {}
            }
        }
        if let Some(s) = current.take() {
            out.insert(s.name.clone(), s);
        }
    }
    commands.sort_by_key(|(ph, _)| *ph); // stable: a phase's commands keep their order
    (out, commands.into_iter().map(|(_, c)| c).collect())
}

/// Set `name=value` in an environment, replacing an earlier value.
fn set_env(envp: &mut Vec<Vec<u8>>, name: &str, value: &str) {
    let prefix = format!("{name}=");
    envp.retain(|e| !e.starts_with(prefix.as_bytes()));
    envp.push(format!("{name}={value}").into_bytes());
}

/// How long `exec_start` waits for its service, at most.
const EXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// init: the services, what runs, and how to start more.
pub struct Init {
    sysroot: PathBuf,
    instance: PathBuf,
    /// The global environment every service is started with (`export`, `load_exports`).
    envp: Mutex<Vec<Vec<u8>>>,
    services: HashMap<String, Service>,
    boot_commands: Vec<Command>,
    running: Mutex<HashMap<String, Weak<Process>>>,
    /// Services never started here, by program: what runs only in a zygote-forked process.
    skip: fn(&Service) -> bool,
}

static INIT: OnceLock<Arc<Init>> = OnceLock::new();

/// The init of this host process, once one is started.
pub fn current() -> Option<Arc<Init>> {
    INIT.get().cloned()
}

impl Init {
    /// Read the services and become this host process's init.
    pub fn start(sysroot: PathBuf, instance: PathBuf, envp: Vec<Vec<u8>>) -> Result<Arc<Self>, String> {
        let root = Sysroot::open(&sysroot)?;
        let (services, boot_commands) = parse(&root);
        // The boot phases' exports are unconditional: the environment has them from the start.
        let mut envp = envp;
        for c in &boot_commands {
            if let Command::Export(name, value) = c {
                set_env(&mut envp, name, value);
            }
        }
        let init = Arc::new(Self {
            sysroot,
            instance,
            envp: Mutex::new(envp),
            services,
            boot_commands,
            running: Mutex::default(),
            // The zygote (app_process): apps are started without it (the C design, decision 4).
            skip: |s| s.argv.first().is_some_and(|p| p.contains("app_process")),
        });
        let _ = INIT.set(Arc::clone(&init));
        Ok(init)
    }

    /// The services, by name.
    #[must_use]
    pub fn services(&self) -> &HashMap<String, Service> {
        &self.services
    }

    /// The global environment a service started now gets.
    #[must_use]
    pub fn environment(&self) -> Vec<Vec<u8>> {
        self.envp.lock().clone()
    }

    /// Boot: the boot phases' commands in order (`start`, `exec_start`, `export`,
    /// `load_exports`), then `class_start` of `classes`. What started.
    pub fn boot(&self, classes: &[&str]) -> Vec<String> {
        let mut started = Vec::new();
        for c in &self.boot_commands {
            match c {
                Command::Start(n) => {
                    if self.start_service(n) {
                        started.push(n.clone());
                    }
                }
                Command::ExecStart(n) => {
                    if let Some(status) = self.exec_service(n) {
                        started.push(n.clone());
                        if std::env::var("OMNI_INIT_TRACE").as_deref() == Ok("1") {
                            eprintln!("[init] exec_start {n}: {status:?}");
                        }
                    }
                }
                Command::Export(name, value) => set_env(&mut self.envp.lock(), name, value),
                Command::LoadExports(path) => self.load_exports(path),
            }
        }
        started.extend(self.class_start(classes));
        started
    }

    /// `load_exports <path>`: the file's `export NAME VALUE` lines (a guest path on a writable
    /// mount, written by a service such as derive_classpath).
    fn load_exports(&self, path: &str) {
        let Some(rel) = path.strip_prefix('/') else { return };
        let Some(host) = crate::vfs::host_path(&self.instance, rel.as_bytes()) else { return };
        let Ok(text) = std::fs::read_to_string(host) else { return };
        let mut envp = self.envp.lock();
        for line in text.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            if let ["export", name, value] = words[..] {
                set_env(&mut envp, name, value);
            }
        }
    }

    /// `exec_start`: start a service and wait for it to end (at most [`EXEC_TIMEOUT`]). Its end,
    /// or `None` when it could not start.
    fn exec_service(&self, name: &str) -> Option<Option<crate::process::ExitStatus>> {
        let service = self.services.get(name)?;
        if (self.skip)(service) {
            return None;
        }
        let config = SpawnConfig {
            sysroot: self.sysroot.clone(),
            instance_dir: self.instance.clone(),
            argv: service.argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            envp: self.environment(),
            stdout: Output::Host,
            stderr: Output::Host,
            trace: false,
        };
        let p = Process::spawn_as(config, service.uid).map_err(|e| eprintln!("[init] {name}: {e}")).ok()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new().name(format!("init-exec-{name}")).spawn(move || tx.send(p.run())).ok()?;
        Some(rx.recv_timeout(EXEC_TIMEOUT).ok())
    }

    /// `class_start <class>` for each class: every service in it that is not `disabled`.
    pub fn class_start(&self, classes: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = self
            .services
            .values()
            .filter(|s| !s.disabled && s.classes.iter().any(|c| classes.contains(&c.as_str())))
            .map(|s| s.name.clone())
            .collect();
        names.sort();
        names.into_iter().filter(|n| self.start_service(n)).collect()
    }

    /// Start a service by name, unless it is running (or cannot run here). Whether it started.
    pub fn start_service(&self, name: &str) -> bool {
        let Some(service) = self.services.get(name) else { return false };
        if (self.skip)(service) {
            return false;
        }
        {
            let running = self.running.lock();
            if running.get(name).is_some_and(|p| p.strong_count() > 0) {
                return false;
            }
        }
        let config = SpawnConfig {
            sysroot: self.sysroot.clone(),
            instance_dir: self.instance.clone(),
            argv: service.argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            envp: self.environment(),
            stdout: Output::Host,
            stderr: Output::Host,
            trace: false,
        };
        match Process::spawn_as(config, service.uid) {
            Ok(p) => {
                self.running.lock().insert(name.to_string(), Arc::downgrade(&p));
                let name = name.to_string();
                std::thread::Builder::new()
                    .name(format!("init-{name}"))
                    .spawn(move || {
                        let status = p.run();
                        if std::env::var("OMNI_INIT_TRACE").as_deref() == Ok("1") {
                            eprintln!("[init] {name} ended: {status:?}");
                        }
                    })
                    .is_ok()
            }
            Err(e) => {
                eprintln!("[init] {name}: {e}");
                false
            }
        }
    }

    /// The service serving `interface` (an AIDL name, or a HIDL `pkg@ver::IFoo/instance`).
    #[must_use]
    pub fn service_for_interface(&self, interface: &str) -> Option<String> {
        let wanted = interface.replace('/', " ");
        self.services
            .values()
            .find(|s| s.interfaces.iter().any(|i| i == &format!("aidl {interface}") || i == &wanted || i.ends_with(&format!(" {interface}"))))
            .map(|s| s.name.clone())
    }

    /// A `ctl.*` property set by a process: init's control messages.
    pub fn control(&self, name: &str, value: &str) -> bool {
        match name {
            "ctl.start" | "ctl.restart" => self.start_service(value),
            "ctl.interface_start" | "ctl.interface_restart" => {
                // "aidl/<name>" or "<hidl fqname>/<instance>"
                let wanted = value.strip_prefix("aidl/").unwrap_or(value);
                self.service_for_interface(wanted).is_some_and(|s| self.start_service(&s))
            }
            _ => true, // stop and the rest: nothing here stops a service yet
        }
    }
}
