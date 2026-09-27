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

/// The services, and the `start`s of the boot-phase `on` blocks, in phase order.
fn parse(sysroot: &Sysroot) -> (HashMap<String, Service>, Vec<String>) {
    let mut scripts: Vec<Vec<u8>> = vec![b"/system/etc/init/hw/init.rc".to_vec()];
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
    let mut starts: Vec<(usize, String)> = Vec::new();
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
                Some("start") if current.is_none() && words.len() == 2 => {
                    if let Some(ph) = phase {
                        starts.push((ph, words[1].to_string()));
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
    starts.sort_by_key(|(ph, _)| *ph);
    (out, starts.into_iter().map(|(_, n)| n).collect())
}

/// init: the services, what runs, and how to start more.
pub struct Init {
    sysroot: PathBuf,
    instance: PathBuf,
    envp: Vec<Vec<u8>>,
    services: HashMap<String, Service>,
    boot_starts: Vec<String>,
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
        let (services, boot_starts) = parse(&root);
        let init = Arc::new(Self {
            sysroot,
            instance,
            envp,
            services,
            boot_starts,
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

    /// Boot: the `start`s of the boot phases, then `class_start` of `classes`. What started.
    pub fn boot(&self, classes: &[&str]) -> Vec<String> {
        let mut started: Vec<String> = self.boot_starts.iter().filter(|n| self.start_service(n)).cloned().collect();
        started.extend(self.class_start(classes));
        started
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
            envp: self.envp.clone(),
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
