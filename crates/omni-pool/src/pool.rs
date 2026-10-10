//! **The pool**: the warm devices and the app instances on them, shared by the app's screen and
//! its MCP server. Every call answers at once; the work (a boot, an install, a start) runs on
//! threads of its own, and [`Pool::snapshot`] shows where each one is.
//!
//! * A launch is placed by [`crate::plan::place`] under the pool's lock -- launches under way count
//!   as running -- so many launches at once still land as the rules say.
//! * One device does one install-and-start at a time: two apps cold-starting together on one
//!   device made one of them miss its input deadline (an ANR, 2026-10-10), one after the other
//!   neither did.
//! * A ticker (every second) follows the devices' boots, the windows people close, the apps that
//!   end on their own, and the idle devices: closed after `idle_close_minutes` unless they are the
//!   `keep_warm` kept warm, and one booted when none is idle.
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use omni_warm::Device;

use crate::config::Config;
use crate::device;
use crate::plan::{self, DeviceId, DeviceView, IdleView, Place};

/// An instance's id in the pool.
pub type InstanceId = u32;

/// Where a device is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceState {
    Booting,
    Ready,
    Closing,
    /// It ended, or never came up: why.
    Gone(String),
}

/// Where an instance is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceState {
    /// Its device is booting.
    Waiting,
    /// Installing, or waiting for the device's previous install-and-start.
    Installing,
    Starting,
    Running,
    Closing,
    /// It ended: why.
    Ended(String),
}

/// One device, as the screen and the MCP server see it.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub dir: PathBuf,
    pub state: DeviceState,
    pub up_for: Duration,
    /// How long it has run no app (`None` while it runs one, or before it is ready).
    pub idle_for: Option<Duration>,
    /// Package -> the SHA-256 of the build installed.
    pub installed: BTreeMap<String, String>,
    pub instances: usize,
}

/// One instance, as the screen and the MCP server see it.
#[derive(Debug, Clone)]
pub struct InstanceInfo {
    pub id: InstanceId,
    pub device: DeviceId,
    pub package: String,
    pub sha256: String,
    pub apk: PathBuf,
    pub user: Option<u32>,
    /// Android's id of its display.
    pub display: Option<i32>,
    /// Its display's screenshot (`<device dir>.d<hwc>.png`), once it has one.
    pub screenshot: Option<PathBuf>,
    pub state: InstanceState,
    pub up_for: Duration,
}

struct DeviceRec {
    dir: PathBuf,
    child: Option<Child>,
    state: DeviceState,
    born: Instant,
    idle_since: Option<Instant>,
    installed: BTreeMap<String, String>,
    /// Users other than 0 the device has (each runs one instance at a time).
    users: Vec<u32>,
    /// One install-and-start at a time.
    busy: Arc<Mutex<()>>,
}

impl DeviceRec {
    fn device(&self) -> Device {
        Device { dir: self.dir.clone() }
    }
}

struct InstanceRec {
    device: DeviceId,
    apk: Arc<omni_warm::Apk>,
    user: Option<u32>,
    hwc: Option<i64>,
    display: Option<i32>,
    state: InstanceState,
    born: Instant,
    /// Ticks in a row its app was not seen on its display.
    missing: u32,
}

#[derive(Default)]
struct State {
    devices: BTreeMap<DeviceId, DeviceRec>,
    instances: BTreeMap<InstanceId, InstanceRec>,
    next_device: DeviceId,
    next_instance: InstanceId,
}

impl State {
    /// An instance takes up its device (and a user there) unless it ended.
    fn live(&self, device: DeviceId) -> impl Iterator<Item = (&InstanceId, &InstanceRec)> {
        self.instances.iter().filter(move |(_, i)| i.device == device && !matches!(i.state, InstanceState::Ended(_)))
    }
}

/// How many lines of the pool's log are kept.
const LOG_LINES: usize = 400;
/// How long a device may take to come up.
const BOOT_LIMIT: Duration = Duration::from_secs(20 * 60);

pub struct Pool {
    state: Mutex<State>,
    config: Mutex<Config>,
    config_path: Option<PathBuf>,
    /// The launcher a device is booted with (`omnidroid aosp --warm`).
    exe: PathBuf,
    /// Where devices live (`omni_warm::root`).
    root: PathBuf,
    log: Mutex<VecDeque<String>>,
    started: Instant,
    stopping: std::sync::atomic::AtomicBool,
}

impl Pool {
    /// A pool with these settings, booting devices with `exe`: the devices this app left running
    /// are adopted, and its ticker started (it warms a device when the settings say so).
    #[must_use]
    pub fn start(config: Config, config_path: Option<PathBuf>, exe: PathBuf) -> Arc<Self> {
        let pool = Arc::new(Self {
            state: Mutex::new(State { next_device: 1, next_instance: 1, ..State::default() }),
            config: Mutex::new(config),
            config_path,
            exe,
            root: omni_warm::root(),
            log: Mutex::new(VecDeque::new()),
            started: Instant::now(),
            stopping: std::sync::atomic::AtomicBool::new(false),
        });
        pool.adopt();
        let ticker = Arc::clone(&pool);
        let _ = std::thread::Builder::new().name("omni-pool-ticker".into()).spawn(move || ticker.tick_forever());
        pool
    }

    /// A line in the pool's log.
    pub fn say(&self, line: impl Into<String>) {
        let secs = self.started.elapsed().as_secs();
        let mut log = self.log.lock();
        log.push_back(format!("{:02}:{:02}:{:02} {}", secs / 3600, secs / 60 % 60, secs % 60, line.into()));
        while log.len() > LOG_LINES {
            log.pop_front();
        }
    }

    /// The pool's log, oldest first.
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        self.log.lock().iter().cloned().collect()
    }

    /// The settings now.
    #[must_use]
    pub fn config(&self) -> Config {
        self.config.lock().clone()
    }

    /// Change a setting, and keep it.
    ///
    /// # Errors
    /// An unknown key, a bad value, or the file could not be written.
    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let mut c = self.config.lock();
        if !c.set(key, value)? {
            return Err(format!("no setting `{key}` (settings lists them)"));
        }
        if let Some(path) = &self.config_path {
            c.save(path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        drop(c);
        self.say(format!("setting {key} = {value}"));
        Ok(())
    }

    /// What there is now.
    #[must_use]
    pub fn snapshot(&self) -> (Vec<DeviceInfo>, Vec<InstanceInfo>) {
        let s = self.state.lock();
        let devices = s
            .devices
            .iter()
            .map(|(id, d)| DeviceInfo {
                id: *id,
                dir: d.dir.clone(),
                state: d.state.clone(),
                up_for: d.born.elapsed(),
                idle_for: d.idle_since.map(|t| t.elapsed()),
                installed: d.installed.clone(),
                instances: s.live(*id).count(),
            })
            .collect();
        let instances = s
            .instances
            .iter()
            .map(|(id, i)| {
                let dir = s.devices.get(&i.device).map(|d| d.dir.clone());
                InstanceInfo {
                    id: *id,
                    device: i.device,
                    package: i.apk.package.clone(),
                    sha256: i.apk.sha256.clone(),
                    apk: i.apk.path.clone(),
                    user: i.user,
                    display: i.display,
                    screenshot: dir.zip(i.hwc).map(|(d, h)| {
                        let stem = d.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                        d.with_file_name(format!("{stem}.d{h}.png"))
                    }),
                    state: i.state.clone(),
                    up_for: i.born.elapsed(),
                }
            })
            .collect();
        (devices, instances)
    }

    /// A device's directory (for its log, its shell).
    #[must_use]
    pub fn device(&self, id: DeviceId) -> Option<Device> {
        self.state.lock().devices.get(&id).filter(|d| !matches!(d.state, DeviceState::Gone(_))).map(DeviceRec::device)
    }

    /// Launch `count` instances of the APK at `apk` -- any APK. Answers their ids at once; each one's
    /// state is in [`Pool::snapshot`].
    ///
    /// # Errors
    /// Not an APK, or one with no launcher Activity; a device limit reached.
    pub fn launch(self: &Arc<Self>, apk: &Path, count: u32) -> Result<Vec<InstanceId>, String> {
        let apk = std::fs::canonicalize(apk).map_err(|e| format!("{}: {e}", apk.display()))?;
        let apk = Arc::new(omni_warm::Apk::read(&apk)?);
        let launcher = apk.launcher.clone().ok_or_else(|| format!("{} has no launcher Activity: nothing to start", apk.package))?;
        let max = self.config.lock().max_devices;
        let mut ids = Vec::new();
        let mut boots = Vec::new();
        {
            let mut s = self.state.lock();
            for _ in 0..count.max(1) {
                let views: Vec<DeviceView> = s
                    .devices
                    .iter()
                    .filter(|(_, d)| matches!(d.state, DeviceState::Booting | DeviceState::Ready))
                    .map(|(id, d)| DeviceView {
                        id: *id,
                        ready: d.state == DeviceState::Ready,
                        installed: d.installed.clone(),
                        running: s.live(*id).map(|(_, i)| i.apk.package.clone()).collect(),
                        closing: false,
                    })
                    .collect();
                let device = match plan::place(&views, &apk.package, &apk.sha256) {
                    Place::Run(d) | Place::Install(d) | Place::Replace(d) => d,
                    Place::Boot => {
                        let alive = s.devices.values().filter(|d| matches!(d.state, DeviceState::Booting | DeviceState::Ready)).count() as u32;
                        if max > 0 && alive >= max {
                            if ids.is_empty() {
                                return Err(format!("{max} devices run already (max_devices): close one, or raise it"));
                            }
                            self.say(format!("max_devices ({max}) reached: {} of {count} launched", ids.len()));
                            break;
                        }
                        let id = s.next_device;
                        s.next_device += 1;
                        let dir = device::new_dir(&self.root);
                        s.devices.insert(id, DeviceRec { dir, child: None, state: DeviceState::Booting, born: Instant::now(), idle_since: None, installed: BTreeMap::new(), users: Vec::new(), busy: Arc::default() });
                        boots.push(id);
                        id
                    }
                };
                // The build is the device's from now on (placed under the lock: the next launch sees it).
                if let Some(d) = s.devices.get_mut(&device) {
                    d.installed.insert(apk.package.clone(), apk.sha256.clone());
                    d.idle_since = None;
                }
                let id = s.next_instance;
                s.next_instance += 1;
                s.instances.insert(id, InstanceRec { device, apk: Arc::clone(&apk), user: None, hwc: None, display: None, state: InstanceState::Waiting, born: Instant::now(), missing: 0 });
                ids.push(id);
            }
        }
        for id in boots {
            self.boot(id);
        }
        for &id in &ids {
            let pool = Arc::clone(self);
            let launcher = launcher.clone();
            let _ = std::thread::Builder::new().name(format!("omni-pool-launch-{id}")).spawn(move || pool.run_launch(id, &launcher));
        }
        self.say(format!("launch {} x{} ({}): instance{} {}", apk.package, ids.len(), &apk.sha256[..12], if ids.len() == 1 { "" } else { "s" }, ids.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(" ")));
        Ok(ids)
    }

    /// Warm a device now (one more than there are).
    pub fn warm(self: &Arc<Self>) -> DeviceId {
        let id = {
            let mut s = self.state.lock();
            let id = s.next_device;
            s.next_device += 1;
            s.devices.insert(id, DeviceRec { dir: device::new_dir(&self.root), child: None, state: DeviceState::Booting, born: Instant::now(), idle_since: None, installed: BTreeMap::new(), users: Vec::new(), busy: Arc::default() });
            id
        };
        self.boot(id);
        id
    }

    fn boot(&self, id: DeviceId) {
        let (dir, config) = {
            let s = self.state.lock();
            let Some(d) = s.devices.get(&id) else { return };
            (d.dir.clone(), self.config.lock().clone())
        };
        match device::boot(&self.exe, &dir, &config) {
            Ok(child) => {
                if let Some(d) = self.state.lock().devices.get_mut(&id) {
                    d.child = Some(child);
                }
                self.say(format!("device D{id} booting ({})", dir.display()));
            }
            Err(e) => {
                self.say(format!("device D{id} could not boot: {e}"));
                self.device_gone(id, &format!("could not boot: {e}"));
            }
        }
    }

    fn set_state(&self, id: InstanceId, state: InstanceState) {
        if let Some(i) = self.state.lock().instances.get_mut(&id) {
            i.state = state;
        }
    }

    /// One launch's work: wait for the device, install, a display and a user, start.
    fn run_launch(self: Arc<Self>, id: InstanceId, launcher: &str) {
        let Some((device_id, apk)) = self.state.lock().instances.get(&id).map(|i| (i.device, Arc::clone(&i.apk))) else { return };
        // The device up.
        let t = Instant::now();
        loop {
            let state = self.state.lock().devices.get(&device_id).map(|d| d.state.clone());
            match state {
                Some(DeviceState::Ready) => break,
                Some(DeviceState::Booting) if t.elapsed() < BOOT_LIMIT => std::thread::sleep(Duration::from_millis(500)),
                Some(DeviceState::Gone(why)) => return self.end(id, &format!("its device D{device_id} is gone: {why}")),
                _ => return self.end(id, &format!("its device D{device_id} did not come up")),
            }
            if matches!(self.state.lock().instances.get(&id).map(|i| &i.state), Some(InstanceState::Closing | InstanceState::Ended(_)) | None) {
                return;
            }
        }
        self.set_state(id, InstanceState::Installing);
        let Some((dev, busy, size)) = self.state.lock().devices.get(&device_id).map(|d| (d.device(), Arc::clone(&d.busy), self.config.lock().instance_size)) else { return };
        let _turn = busy.lock();
        if matches!(self.state.lock().instances.get(&id).map(|i| &i.state), Some(InstanceState::Closing | InstanceState::Ended(_)) | None) {
            return;
        }
        // Installed unless the device holds these bytes; another build of the package (none of it
        // running: the placement saw to that) is replaced.
        let held = dev.installed_apk(&apk.package).and_then(|p| omni_warm::sha256_file(&p).ok());
        if held.as_deref() != Some(apk.sha256.as_str()) {
            match device::install(&dev, &apk) {
                Ok(i) => self.say(format!("#{id} {} {} on D{device_id} in {:.1} s", apk.package, i.action, i.seconds)),
                Err(e) => return self.end(id, &format!("install: {e}")),
            }
        }
        // Its user: 0 unless an instance of the package runs as 0 there, else a user of its own.
        let user = {
            let s = self.state.lock();
            let taken: Vec<u32> = s.live(device_id).filter(|(other, _)| **other != id).filter_map(|(_, i)| i.user.filter(|u| *u == 0 && i.apk.package == apk.package).or(i.user.filter(|u| *u != 0))).collect();
            let users = s.devices.get(&device_id).map(|d| d.users.clone()).unwrap_or_default();
            if !taken.contains(&0) {
                Some(0)
            } else {
                users.into_iter().find(|u| !taken.contains(u))
            }
        };
        let user = match user {
            Some(u) => u,
            None => match device::create_user(&dev) {
                Ok(u) => {
                    if let Some(d) = self.state.lock().devices.get_mut(&device_id) {
                        d.users.push(u);
                    }
                    self.say(format!("D{device_id}: user {u} made for #{id}"));
                    u
                }
                Err(e) => return self.end(id, &format!("a user for it: {e}")),
            },
        };
        if let Some(i) = self.state.lock().instances.get_mut(&id) {
            i.user = Some(user);
            i.state = InstanceState::Starting;
        }
        let (hwc, display) = match device::add_display(&dev, size) {
            Ok(d) => d,
            Err(e) => return self.end(id, &format!("its display: {e}")),
        };
        if let Some(i) = self.state.lock().instances.get_mut(&id) {
            (i.hwc, i.display) = (Some(hwc), Some(display));
        }
        let t = Instant::now();
        match device::start(&dev, &apk.package, launcher, user, display) {
            Ok(_) => {
                let closing = {
                    let mut s = self.state.lock();
                    match s.instances.get_mut(&id) {
                        Some(i) if i.state == InstanceState::Starting => {
                            i.state = InstanceState::Running;
                            false
                        }
                        _ => true,
                    }
                };
                self.say(format!("#{id} {} running on D{device_id} (user {user}, display {display}) in {:.1} s", apk.package, t.elapsed().as_secs_f64()));
                if closing {
                    self.cleanup(id);
                }
            }
            Err(e) => self.end(id, &format!("start: {e}")),
        }
    }

    /// End an instance with `why`, and give back what it held.
    fn end(&self, id: InstanceId, why: &str) {
        self.say(format!("#{id} ended: {why}"));
        self.cleanup(id);
        if let Some(i) = self.state.lock().instances.get_mut(&id) {
            i.state = InstanceState::Ended(why.to_string());
        }
        self.mark_idle();
    }

    /// Stop an instance's app and user, and remove its display.
    fn cleanup(&self, id: InstanceId) {
        let Some((dev, package, user, hwc, busy)) = ({
            let s = self.state.lock();
            s.instances.get(&id).and_then(|i| s.devices.get(&i.device).map(|d| (d.device(), i.apk.package.clone(), i.user, i.hwc, Arc::clone(&d.busy))))
        }) else {
            return;
        };
        if !dev.alive() {
            return;
        }
        let _turn = busy.lock();
        if let Some(u) = user {
            if let Err(e) = device::stop(&dev, &package, u) {
                self.say(format!("#{id}: stopping it: {e}"));
            }
        }
        if let Some(h) = hwc {
            let _ = device::remove_display(&dev, h);
            let _ = std::fs::remove_file(dev.ctl().join(format!("display-{h}.closed")));
        }
        if let Some(i) = self.state.lock().instances.get_mut(&id) {
            i.hwc = None;
        }
    }

    /// Close an instance: its app stopped, its window gone.
    ///
    /// # Errors
    /// No such instance, or it ended already.
    pub fn close(self: &Arc<Self>, id: InstanceId) -> Result<(), String> {
        {
            let mut s = self.state.lock();
            let i = s.instances.get_mut(&id).ok_or_else(|| format!("no instance #{id}"))?;
            if matches!(i.state, InstanceState::Ended(_)) {
                return Err(format!("#{id} ended already"));
            }
            i.state = InstanceState::Closing;
        }
        let pool = Arc::clone(self);
        let _ = std::thread::Builder::new().name(format!("omni-pool-close-{id}")).spawn(move || pool.end(id, "closed"));
        Ok(())
    }

    /// Forget the instances that ended.
    pub fn clear_ended(&self) {
        self.state.lock().instances.retain(|_, i| !matches!(i.state, InstanceState::Ended(_)));
    }

    /// Stop a device and end its instances.
    ///
    /// # Errors
    /// No such device.
    pub fn kill(&self, id: DeviceId) -> Result<(), String> {
        let (dev, child) = {
            let mut s = self.state.lock();
            let d = s.devices.get_mut(&id).ok_or_else(|| format!("no device D{id}"))?;
            d.state = DeviceState::Closing;
            (d.device(), d.child.take())
        };
        self.say(format!("device D{id} stopping"));
        dev.stop();
        if let Some(mut child) = child {
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(20) && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(200));
            }
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(device::marker(&dev.dir));
        self.device_gone(id, "stopped");
        Ok(())
    }

    fn device_gone(&self, id: DeviceId, why: &str) {
        let mut s = self.state.lock();
        if let Some(d) = s.devices.get_mut(&id) {
            d.state = DeviceState::Gone(why.to_string());
        }
        for i in s.instances.values_mut().filter(|i| i.device == id && !matches!(i.state, InstanceState::Ended(_))) {
            i.state = InstanceState::Ended(format!("its device D{id} {why}"));
        }
        s.devices.retain(|_, d| !matches!(d.state, DeviceState::Gone(_)));
    }

    /// Stop everything (when the app quits with `stop_on_exit`).
    pub fn shutdown(&self, stop_devices: bool) {
        self.stopping.store(true, std::sync::atomic::Ordering::Release);
        if !stop_devices {
            return;
        }
        let ids: Vec<DeviceId> = self.state.lock().devices.keys().copied().collect();
        let threads: Vec<_> = ids
            .into_iter()
            .filter_map(|id| {
                let dev = self.device(id)?;
                Some(std::thread::spawn(move || dev.stop()))
            })
            .collect();
        for t in threads {
            let _ = t.join();
        }
        let mut s = self.state.lock();
        for d in s.devices.values_mut() {
            if let Some(mut c) = d.child.take() {
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(20) && matches!(c.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(200));
                }
                let _ = c.kill();
                let _ = c.wait();
            }
            let _ = std::fs::remove_file(device::marker(&d.dir));
        }
    }

    /// A ready device with no instance left is idle from now.
    fn mark_idle(&self) {
        let mut s = self.state.lock();
        let busy: Vec<DeviceId> = s.instances.values().filter(|i| !matches!(i.state, InstanceState::Ended(_))).map(|i| i.device).collect();
        for (id, d) in &mut s.devices {
            if d.state == DeviceState::Ready && !busy.contains(id) && d.idle_since.is_none() {
                d.idle_since = Some(Instant::now());
            } else if busy.contains(id) {
                d.idle_since = None;
            }
        }
    }

    /// The devices this app left running (`<dir>.pool`), taken back as idle devices: their extra
    /// displays removed, their users and builds read.
    fn adopt(&self) {
        for dev in omni_warm::find_all() {
            if !device::marker(&dev.dir).exists() || !dev.ready() {
                continue;
            }
            for hwc in device::extra_displays(&dev) {
                let _ = device::remove_display(&dev, hwc);
            }
            let installed: BTreeMap<String, String> = dev
                .installed_packages()
                .into_iter()
                .filter_map(|p| Some((p.clone(), omni_warm::sha256_file(&dev.installed_apk(&p)?).ok()?)))
                .collect();
            let users: Vec<u32> = device::list_users(&dev).unwrap_or_default().into_iter().filter(|u| *u != 0).collect();
            let mut s = self.state.lock();
            let id = s.next_device;
            s.next_device += 1;
            s.devices.insert(id, DeviceRec { dir: dev.dir.clone(), child: None, state: DeviceState::Ready, born: Instant::now(), idle_since: Some(Instant::now()), installed, users, busy: Arc::default() });
            drop(s);
            self.say(format!("device D{id} adopted ({}), left running by the app before", dev.dir.display()));
        }
    }

    fn tick_forever(self: Arc<Self>) {
        let mut n = 0u64;
        while !self.stopping.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(Duration::from_secs(1));
            n += 1;
            self.tick(n);
        }
    }

    fn tick(self: &Arc<Self>, n: u64) {
        // Boots and deaths.
        let mut came = Vec::new();
        let mut gone = Vec::new();
        {
            let mut s = self.state.lock();
            for (id, d) in &mut s.devices {
                let exited = d.child.as_mut().is_some_and(|c| !matches!(c.try_wait(), Ok(None)));
                match d.state {
                    DeviceState::Booting if d.device().ready() => {
                        d.state = DeviceState::Ready;
                        d.idle_since = Some(Instant::now());
                        came.push((*id, d.born.elapsed()));
                    }
                    DeviceState::Booting if exited => gone.push((*id, format!("ended while booting (its log: {})", d.dir.with_extension("launcher.log").display()))),
                    DeviceState::Booting if d.born.elapsed() > BOOT_LIMIT => gone.push((*id, "did not come up".to_string())),
                    DeviceState::Ready if exited || (d.child.is_none() && !d.device().alive()) => gone.push((*id, "ended".to_string())),
                    _ => {}
                }
            }
        }
        for (id, took) in came {
            self.say(format!("device D{id} ready in {:.0} s", took.as_secs_f64()));
        }
        for (id, why) in gone {
            self.say(format!("device D{id} {why}"));
            self.device_gone(id, &why);
        }
        self.mark_idle();
        // Windows closed by the person: their instances close.
        let closed: Vec<InstanceId> = {
            let s = self.state.lock();
            s.instances
                .iter()
                .filter(|(_, i)| i.state == InstanceState::Running)
                .filter(|(_, i)| s.devices.get(&i.device).zip(i.hwc).is_some_and(|(d, h)| d.device().ctl().join(format!("display-{h}.closed")).exists()))
                .map(|(id, _)| *id)
                .collect()
        };
        for id in closed {
            self.say(format!("#{id}: its window was closed"));
            let _ = self.close(id);
        }
        // Apps that ended on their own (every 5 s): not on their display twice in a row.
        if n % 5 == 0 {
            self.check_running();
        }
        // Idle devices out; a warm one in.
        let (views, config) = {
            let s = self.state.lock();
            let views: Vec<IdleView> = s
                .devices
                .iter()
                .filter(|(_, d)| matches!(d.state, DeviceState::Booting | DeviceState::Ready))
                .map(|(id, d)| IdleView { id: *id, ready: d.state == DeviceState::Ready, running: s.live(*id).count(), idle_for: d.idle_since.map(|t| t.elapsed()) })
                .collect();
            (views, self.config.lock().clone())
        };
        for id in plan::to_close(&views, config.idle_close_minutes, config.keep_warm) {
            self.say(format!("device D{id} idle {} min: closing it", config.idle_close_minutes));
            let pool = Arc::clone(self);
            let _ = std::thread::spawn(move || pool.kill(id));
        }
        if config.warm_on_start {
            for _ in 0..plan::to_warm(&views, config.keep_warm, config.max_devices) {
                let id = self.warm();
                self.say(format!("device D{id}: warming one, so the next launch starts at once"));
            }
        }
    }

    fn check_running(self: &Arc<Self>) {
        let devices: Vec<(DeviceId, Device)> = {
            let s = self.state.lock();
            s.devices.iter().filter(|(id, d)| d.state == DeviceState::Ready && s.live(**id).any(|(_, i)| i.state == InstanceState::Running)).map(|(id, d)| (*id, d.device())).collect()
        };
        for (device_id, dev) in devices {
            let Ok(seen) = device::running(&dev) else { continue };
            let mut ended = Vec::new();
            {
                let mut s = self.state.lock();
                for (id, i) in s.instances.iter_mut().filter(|(_, i)| i.device == device_id && i.state == InstanceState::Running) {
                    let here = matches!((i.display, i.user), (Some(d), Some(u)) if seen.iter().any(|(sd, su, p)| *sd == d && *su == u && *p == i.apk.package));
                    i.missing = if here { 0 } else { i.missing + 1 };
                    if i.missing >= 2 {
                        ended.push(*id);
                    }
                }
            }
            for id in ended {
                self.end(id, "the app stopped");
            }
        }
    }
}
