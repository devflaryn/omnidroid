//! The warm device: one Android per host, booted and idle with no app of its own (`omnidroid aosp
//! --warm`), that APKs are installed on, started, stopped and uninstalled -- one after another --
//! through its control channel, never a boot per APK.
//!
//! - **Found, not made, when it is there.** A warm device is `<temp>/omni-warm-<secs>`, alive while
//!   its control channel's heartbeat (`<dir>.ctl/alive`) is fresh and nothing asked it to stop. Any
//!   server on the host uses the one that is up; a device is booted only when none is, under a lock
//!   (`<temp>/omni-warm.lock`), so two servers never boot two.
//! - **The control channel** (`omni-linux-run --control`): a shell command written as
//!   `<dir>.ctl/<id>.cmd` is run as the shell user beside the device, its output in `<id>.out` and
//!   status in `<id>.rc` -- `adb shell` without adb.
//! - **Decided by content, not version.** An APK is known by the SHA-256 of its bytes, compared with
//!   the `base.apk` the device holds for that package (`/data/app/*/<package>-*/base.apk`, a copy of
//!   the installed bytes): the same bytes are reused as installed; other bytes -- even with the same
//!   versionCode and name -- are installed again (`pm install -r -d`, and uninstalled first when the
//!   signature differs); another package's test app is uninstalled first. The package and the
//!   launcher Activity come from the APK's own manifest.
//!
//! Used by the MCP server (`omni-mcp`'s `start_instance`/`install_apk` without an account) and by
//! the `omnidroid` launcher (`omnidroid aosp --cookie --place` on a warm device that is up).
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// The prefix of a warm device's directory in the temp directory.
pub const PREFIX: &str = "omni-warm-";
const LOCK: &str = "omni-warm.lock";
/// A lock older than this is a boot that died: a new device may be booted.
const LOCK_STALE: Duration = Duration::from_secs(15 * 60);
/// Where APKs are handed to the device (guest `/data/local/tmp/omni-apk`).
const DROP: &str = "data/local/tmp/omni-apk";

/// Where a rooted warm device's session records its root profile's hash (in the device directory).
pub const ROOT_HASH_FILE: &str = "root-hash";
/// The host-only root profile `omni-linux`'s `root::install::stage` writes in the instance directory.
const PROFILE_FILE: &str = ".omni-root-profile";

/// A warm device, by its directory.
#[derive(Debug, Clone)]
pub struct Device {
    pub dir: PathBuf,
}

/// What `ensure` found: a device ready for apps, or one still booting.
pub enum Found {
    Ready(Device),
    Booting(Device),
}

impl Device {
    fn ctl(&self) -> PathBuf {
        self.dir.with_extension("ctl")
    }

    fn tmp(&self, name: &str) -> PathBuf {
        self.dir.join("data/local/tmp").join(name)
    }

    /// Its control channel answers: the heartbeat written in the last 5 s.
    #[must_use]
    pub fn alive(&self) -> bool {
        fresh(&self.ctl().join("alive"), Duration::from_secs(5)) && !self.tmp("stop").exists()
    }

    /// Booted and set up: an app can be installed and started.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.alive() && self.tmp("warm-ready").exists()
    }

    /// The root profile the device was made with: the hash its session recorded in `root-hash` at
    /// creation (`omni-linux`'s `root::key`), `None` for an unrooted device.
    #[must_use]
    pub fn root_hash(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join(ROOT_HASH_FILE)).ok()?;
        Some(text.trim().to_string()).filter(|h| !h.is_empty())
    }

    /// The device's root state from its host-only profile (`.omni-root-profile`): `Some(module ids)`
    /// when it is rooted, `None` when it is not.
    #[must_use]
    pub fn root_modules(&self) -> Option<Vec<String>> {
        let text = std::fs::read_to_string(self.dir.join(PROFILE_FILE)).ok()?;
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        if !lines.contains(&"root=1") {
            return None;
        }
        Some(lines.iter().filter_map(|l| l.strip_prefix("module=")).map(str::to_string).collect())
    }

    /// The session's log (`<dir>.log`).
    #[must_use]
    pub fn log(&self) -> PathBuf {
        self.dir.with_extension("log")
    }

    /// The display's latest frame (`<dir>.png`).
    #[must_use]
    pub fn screenshot(&self) -> PathBuf {
        self.dir.with_extension("png")
    }

    /// Run `command` on the device as `uid` (the shell's, 2000, by default): its exit status and
    /// output (stdout and stderr), within `limit`.
    ///
    /// # Errors
    /// The device's control channel is gone, or the command did not end within `limit`.
    pub fn shell_as(&self, command: &str, uid: Option<u32>, limit: Duration) -> Result<(i64, String), String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let ctl = self.ctl();
        if !ctl.join("pid").exists() {
            return Err(format!("the device's control channel ({}) is not up", ctl.display()));
        }
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let id = format!("{now:024}-{}-{n}", std::process::id());
        let text = match uid {
            Some(u) => format!("#uid={u}\n{command}"),
            None => command.to_string(),
        };
        let part = ctl.join(format!("{id}.part"));
        std::fs::write(&part, text).map_err(|e| format!("{}: {e}", part.display()))?;
        std::fs::rename(&part, ctl.join(format!("{id}.cmd"))).map_err(|e| format!("{}: {e}", ctl.display()))?;
        let (rc, out) = (ctl.join(format!("{id}.rc")), ctl.join(format!("{id}.out")));
        let deadline = Instant::now() + limit;
        loop {
            if let Ok(code) = std::fs::read_to_string(&rc) {
                let output = std::fs::read(&out).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
                let _ = std::fs::remove_file(&rc);
                let _ = std::fs::remove_file(&out);
                return Ok((code.trim().parse().unwrap_or(-1), output));
            }
            if Instant::now() > deadline {
                return Err(format!("`{}` did not end within {} s", command.chars().take(80).collect::<String>(), limit.as_secs()));
            }
            if !self.alive() {
                return Err("the device stopped".into());
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    /// `shell_as` as the shell user.
    ///
    /// # Errors
    /// As `shell_as`.
    pub fn shell(&self, command: &str, limit: Duration) -> Result<(i64, String), String> {
        self.shell_as(command, None, limit)
    }

    /// Ask the device to shut down (its `stop` file); its session removes it.
    pub fn stop(&self) {
        let _ = std::fs::create_dir_all(self.dir.join("data/local/tmp"));
        let _ = std::fs::write(self.tmp("stop"), "1");
    }

    /// The apps installed on the device (not the image's): `/data/app/<random>/<package>-<random>/`,
    /// each with its `base.apk`. (A package name has no `-`; the random part can.)
    #[must_use]
    pub fn installed_packages(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(self.dir.join("data/app"))
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|outer| std::fs::read_dir(outer.path()).into_iter().flatten().flatten())
            .filter(|inner| inner.path().join("base.apk").is_file())
            .filter_map(|inner| inner.file_name().to_string_lossy().split('-').next().map(str::to_string))
            .filter(|p| !p.is_empty())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The APK the device holds for `package` (the installed bytes), if it has one.
    #[must_use]
    pub fn installed_apk(&self, package: &str) -> Option<PathBuf> {
        let prefix = format!("{package}-");
        std::fs::read_dir(self.dir.join("data/app")).ok()?.flatten().find_map(|outer| {
            std::fs::read_dir(outer.path()).ok()?.flatten().find_map(|inner| {
                let name = inner.file_name().to_string_lossy().into_owned();
                let base = inner.path().join("base.apk");
                (name.starts_with(&prefix) && base.is_file()).then_some(base)
            })
        })
    }
}

/// Where warm devices live: `OMNI_MCP_WARM_DIR`, else the temp directory. (A Linux host whose
/// `/tmp` is a tmpfs names a directory on disk: a device's `/data` is ~1 GiB.)
#[must_use]
pub fn root() -> PathBuf {
    std::env::var_os("OMNI_MCP_WARM_DIR").map_or_else(std::env::temp_dir, PathBuf::from)
}

/// `<prefix><digits>`: an instance's own directory, not a sibling of it (`<dir>.ctl`, the control
/// channel's, is a directory too).
#[must_use]
pub fn numbered(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Written within `age`.
fn fresh(path: &Path, age: Duration) -> bool {
    std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a < age)
}

/// The live warm device on this host, if there is one (the newest).
#[must_use]
pub fn find() -> Option<Device> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(root())
        .ok()?
        .flatten()
        .filter(|e| numbered(&e.file_name().to_string_lossy(), PREFIX) && e.path().is_dir())
        .map(|e| e.path())
        .filter(|d| Device { dir: d.clone() }.alive())
        .collect();
    found.sort();
    found.pop().map(|dir| Device { dir })
}

/// The device being booted under the lock, if a boot is under way (and not stale).
fn booting() -> Option<Device> {
    let lock = root().join(LOCK);
    let dir = PathBuf::from(std::fs::read_to_string(&lock).ok()?.trim());
    if fresh(&lock, LOCK_STALE) {
        Some(Device { dir })
    } else {
        let _ = std::fs::remove_file(&lock);
        None
    }
}

/// The warm device: the live one, or the one another server is booting, or a new one booted by
/// `boot` (given the directory to boot it in) -- never a second device.
///
/// # Errors
/// `boot`'s error.
pub fn ensure(boot: impl FnOnce(&Path) -> Result<(), String>) -> Result<Found, String> {
    if let Some(d) = find() {
        return Ok(if d.ready() { Found::Ready(d) } else { Found::Booting(d) });
    }
    if let Some(d) = booting() {
        return Ok(Found::Booting(d));
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let dir = root().join(format!("{PREFIX}{secs}"));
    let lock = root().join(LOCK);
    // create_new: of two servers asking at once, one boots.
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
        Ok(mut f) => {
            use std::io::Write;
            let _ = write!(f, "{}", dir.display());
        }
        Err(_) => return booting().map(Found::Booting).ok_or_else(|| "another server is booting the warm device".to_string()),
    }
    if let Err(e) = boot(&dir) {
        let _ = std::fs::remove_file(&lock);
        return Err(e);
    }
    Ok(Found::Booting(Device { dir }))
}

/// Wait for `device` to be ready, at most `limit`; the lock is released once it is.
///
/// # Errors
/// It stopped, or it was not ready within `limit`.
pub fn wait_ready(device: &Device, limit: Duration) -> Result<(), String> {
    let deadline = Instant::now() + limit;
    let started = Instant::now();
    // The boot lock, when it names this device: gone once the device is ready, or its boot failed.
    let unlock = || {
        let lock = root().join(LOCK);
        if std::fs::read_to_string(&lock).is_ok_and(|d| Path::new(d.trim()) == device.dir) {
            let _ = std::fs::remove_file(&lock);
        }
    };
    loop {
        if device.ready() {
            unlock();
            return Ok(());
        }
        // Its log appears within seconds of the launch, and its control channel with the boot.
        if started.elapsed() > Duration::from_secs(120) && !device.log().exists() && !device.dir.with_extension("1.log").exists() {
            unlock();
            return Err(format!("the warm device never started (no {}): is the build current?", device.log().display()));
        }
        if Instant::now() > deadline {
            return Err(format!("the warm device was not ready within {} s (log: {})", limit.as_secs(), device.log().display()));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The SHA-256 of a file's bytes, as hex -- read every time: a rebuilt APK can keep its size and,
/// within a clock tick, its time (~0.1 s for Roblox's 160 MB).
///
/// # Errors
/// The file cannot be read.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// An APK as the device will know it.
#[derive(Debug, Clone)]
pub struct Apk {
    pub path: PathBuf,
    pub sha256: String,
    pub package: String,
    pub launcher: Option<String>,
    pub version_code: Option<u32>,
}

impl Apk {
    /// Read `path`: its bytes' hash and its manifest's package and launcher.
    ///
    /// # Errors
    /// The file cannot be read or is not an APK.
    pub fn read(path: &Path) -> Result<Self, String> {
        let info = omni_apk::launch_info_of(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { path: path.to_path_buf(), sha256: sha256_file(path)?, package: info.package, launcher: info.launcher, version_code: info.version_code })
    }
}

/// How an install went.
#[derive(Debug, Clone, Default)]
pub struct Installed {
    /// `reused` (the same bytes were installed), `installed`, `reinstalled` (other bytes over the
    /// same package), `reinstalled-after-uninstall` (another signature).
    pub action: &'static str,
    /// Other test apps uninstalled first.
    pub uninstalled: Vec<String>,
    pub seconds: f64,
    /// What `pm` said, when it installed.
    pub pm: String,
}

/// The `pm install` options: replace, allow a downgrade, grant what it asks for. `OMNI_MCP_PM_FLAGS`
/// adds more (a lever measured, e.g. `--skip-verification`).
fn pm_install_flags() -> String {
    let extra = std::env::var("OMNI_MCP_PM_FLAGS").unwrap_or_default();
    format!("-r -d -g {extra}").trim_end().to_string()
}

/// Make `apk` the device's test app: reuse it when the device holds the same bytes, else install it
/// (uninstalling any other test app first, and this package first when its signature differs).
///
/// # Errors
/// A control-channel failure, or `pm install` failing.
pub fn install(device: &Device, apk: &Apk) -> Result<Installed, String> {
    install_then(device, apk, "")
}

/// [`install`], and `then` (shell, as root) in the same command once the install succeeded -- one
/// round trip of the control channel, not two (each is ~0.5-1.5 s). `then` is not run when the
/// installed bytes are reused (`action == "reused"`): the caller runs it.
///
/// # Errors
/// As [`install`].
pub fn install_then(device: &Device, apk: &Apk, then: &str) -> Result<Installed, String> {
    let t = Instant::now();
    let limit = Duration::from_secs(300);
    // What the device holds is read from its /data on the host: no round trip to decide.
    let installed = device.installed_packages();
    let others: Vec<String> = installed.iter().filter(|p| **p != apk.package).cloned().collect();
    let held = installed.iter().any(|p| *p == apk.package);
    if held && others.is_empty() {
        if let Some(base) = device.installed_apk(&apk.package) {
            if sha256_file(&base).is_ok_and(|h| h == apk.sha256) {
                return Ok(Installed { action: "reused", uninstalled: others, seconds: t.elapsed().as_secs_f64(), pm: String::new() });
            }
        }
    }
    // Handed to the device by its hash, and gone once installed.
    let drop = device.dir.join(DROP);
    std::fs::create_dir_all(&drop).map_err(|e| format!("{}: {e}", drop.display()))?;
    let name = format!("{}.apk", &apk.sha256[..16]);
    std::fs::copy(&apk.path, drop.join(&name)).map_err(|e| format!("the APK to the device: {e}"))?;
    let guest = format!("/data/local/tmp/omni-apk/{name}");
    let flags = pm_install_flags();
    // Other test apps go first, in the same command.
    let first: String = others.iter().map(|p| format!("pm uninstall {p} >/dev/null 2>&1; ")).collect();
    // "All files access", granted as the device's owner grants it in Settings -- as `pm install -g`
    // grants the runtime permissions -- in the same command (a round trip is ~0.5-1.5 s). An app that
    // asks for it otherwise opens Settings' page for it instead of its own screen (the Roblox APK
    // does, 2026-10-01); for one that does not, a no-op.
    let mut grant = format!(" && appops set {} MANAGE_EXTERNAL_STORAGE allow", apk.package);
    let uid = if then.is_empty() {
        None
    } else {
        grant.push_str(&format!(" && {{ {then}\n}}"));
        Some(0)
    };
    let (mut code, mut said) = device.shell_as(&format!("{first}pm install {flags} {guest}{grant}"), uid, limit)?;
    let mut action = if held { "reinstalled" } else { "installed" };
    // Other bytes of the installed package that pm refuses to put over it -- another signature, or
    // a lower versionCode (`-d` only lets a debuggable app go down) -- replace it: uninstalled first.
    let refused = ["INSTALL_FAILED_UPDATE_INCOMPATIBLE", "signatures do not match", "INSTALL_FAILED_VERSION_DOWNGRADE"];
    if code != 0 && refused.iter().any(|r| said.contains(r)) {
        let (c, s) = device.shell_as(&format!("pm uninstall {} >/dev/null 2>&1; pm install {flags} {guest}{grant}", apk.package), uid, limit)?;
        (code, said, action) = (c, s, "reinstalled-after-uninstall");
    }
    let _ = std::fs::remove_file(drop.join(&name));
    if code != 0 || !said.contains("Success") {
        return Err(format!("pm install {}: {}", apk.path.display(), said.trim()));
    }
    Ok(Installed { action, uninstalled: others, seconds: t.elapsed().as_secs_f64(), pm: said.trim().to_string() })
}

/// How a start went: `am start -W`'s answer (the Activity displayed).
#[derive(Debug, Clone, Default)]
pub struct Started {
    pub component: String,
    pub seconds: f64,
    /// `TotalTime` from `am start -W`, ms: Android's own measure of the launch.
    pub total_time_ms: Option<u64>,
    pub status: String,
    pub output: String,
}

/// Start `apk`'s launcher Activity and wait until it is displayed (`am start -W`).
///
/// # Errors
/// The APK has no launcher Activity, or the start failed.
pub fn start(device: &Device, apk: &Apk, limit: Duration) -> Result<Started, String> {
    let class = apk.launcher.as_deref().ok_or_else(|| format!("{} has no launcher Activity (MAIN/LAUNCHER)", apk.package))?;
    let component = format!("{}/{class}", apk.package);
    let t = Instant::now();
    let (code, out) = device.shell(&format!("input keyevent KEYCODE_WAKEUP; am start -W -n {component}"), limit)?;
    let field = |name: &str| out.lines().find_map(|l| l.trim().strip_prefix(name).map(|v| v.trim().to_string()));
    let status = field("Status:").unwrap_or_default();
    if code != 0 || out.contains("Error") {
        return Err(format!("am start -n {component}: {}", out.trim()));
    }
    Ok(Started { component, seconds: t.elapsed().as_secs_f64(), total_time_ms: field("TotalTime:").and_then(|v| v.parse().ok()), status, output: out.trim().to_string() })
}

/// Stop `package` and clear its data: the device stays warm for the next app.
///
/// # Errors
/// A control-channel failure.
pub fn stop_app(device: &Device, package: &str) -> Result<String, String> {
    let (_, out) = device.shell(&format!("am force-stop {package}; pm clear {package}"), Duration::from_secs(120))?;
    Ok(out.trim().to_string())
}

/// Where a host file is handed to the device as `name`: the host path to write it at (its
/// directory made) and the guest path the device sees it at (`/data/local/tmp/omni-apk/<name>`).
/// The guest command that uses it removes it.
///
/// # Errors
/// The directory could not be made.
pub fn hand_over_path(device: &Device, name: &str) -> Result<(PathBuf, String), String> {
    let drop = device.dir.join(DROP);
    std::fs::create_dir_all(&drop).map_err(|e| format!("{}: {e}", drop.display()))?;
    Ok((drop.join(name), format!("/data/local/tmp/omni-apk/{name}")))
}

/// Start `apk`'s launcher Activity without waiting for it to be displayed, after `before` (shell
/// run first in the same command, as root: files put into the app's data before its first start).
/// The caller follows the start in the device's log ([`LogTail`]).
///
/// # Errors
/// The APK has no launcher Activity, the control channel failed, or `am start` failed.
pub fn launch(device: &Device, apk: &Apk, before: &str) -> Result<String, String> {
    let (code, out) = device.shell_as(&format!("{before}{}", launch_command(apk)?), Some(0), Duration::from_secs(120))?;
    if code != 0 || out.contains("Error") {
        return Err(format!("{}: {}", launch_command(apk)?, out.trim()));
    }
    Ok(out.trim().to_string())
}

/// The shell that starts `apk`'s launcher Activity without waiting for it (the screen woken first).
///
/// # Errors
/// The APK has no launcher Activity.
pub fn launch_command(apk: &Apk) -> Result<String, String> {
    let class = apk.launcher.as_deref().ok_or_else(|| format!("{} has no launcher Activity (MAIN/LAUNCHER)", apk.package))?;
    Ok(format!("input keyevent KEYCODE_WAKEUP; am start -n {}/{class}", apk.package))
}

/// The lines a log file gains from now on, read as they come (a warm device's `<dir>.log`, which
/// every guest log line reaches). A file that shrank was started again: it is read from its start.
pub struct LogTail {
    path: PathBuf,
    pos: u64,
}

impl LogTail {
    /// `path` from its current end.
    #[must_use]
    pub fn from_end(path: &Path) -> Self {
        Self { path: path.to_path_buf(), pos: std::fs::metadata(path).map_or(0, |m| m.len()) }
    }

    /// The whole lines written since the last call.
    pub fn lines(&mut self) -> Vec<String> {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(&self.path) else { return Vec::new() };
        let len = f.metadata().map_or(0, |m| m.len());
        if len < self.pos {
            self.pos = 0;
        }
        let mut data = Vec::new();
        if f.seek(SeekFrom::Start(self.pos)).is_err() || f.read_to_end(&mut data).is_err() {
            return Vec::new();
        }
        let whole = data.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        self.pos += whole as u64;
        String::from_utf8_lossy(&data[..whole]).lines().map(str::to_string).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_known_by_its_bytes() {
        let dir = std::env::temp_dir().join(format!("omni-mcp-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let (a, b) = (dir.join("a.apk"), dir.join("b.apk"));
        std::fs::write(&a, b"abc").expect("a");
        std::fs::write(&b, b"abc").expect("b");
        assert_eq!(sha256_file(&a).expect("a"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha256_file(&a), sha256_file(&b), "the same bytes, another path");
        std::fs::write(&b, b"abd").expect("b");
        assert_ne!(sha256_file(&a), sha256_file(&b), "other bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_is_read_by_whole_lines_from_where_it_was_and_again_when_it_restarts() {
        let path = std::env::temp_dir().join(format!("omni-warm-tail-{}.log", std::process::id()));
        std::fs::write(&path, "before\n").expect("log");
        let mut tail = LogTail::from_end(&path);
        assert!(tail.lines().is_empty(), "what was there before is not read");
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).expect("append");
        std::io::Write::write_all(&mut f, b"one\ntw").expect("write");
        assert_eq!(tail.lines(), ["one"], "a partial line waits");
        std::io::Write::write_all(&mut f, b"o\n").expect("write");
        assert_eq!(tail.lines(), ["two"]);
        drop(f);
        std::fs::write(&path, "new\n").expect("a new log");
        assert_eq!(tail.lines(), ["new"], "a log started again is read from its start");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_instance_is_its_numbered_directory_not_its_control_channel() {
        assert!(numbered("omni-warm-1790806314", PREFIX));
        assert!(!numbered("omni-warm-1790806314.ctl", PREFIX));
        assert!(!numbered("omni-warm-", PREFIX));
        assert!(!numbered("omni-warm.lock", PREFIX));
    }

    #[test]
    fn the_installed_apk_is_found_under_its_package() {
        let dir = std::env::temp_dir().join(format!("omni-mcp-dev-{}", std::process::id()));
        let base = dir.join("data/app/~~x==/com.example.a-y==/base.apk");
        std::fs::create_dir_all(base.parent().unwrap()).expect("dirs");
        std::fs::write(&base, b"apk").expect("base.apk");
        let d = Device { dir: dir.clone() };
        assert_eq!(d.installed_apk("com.example.a"), Some(base));
        assert_eq!(d.installed_apk("com.example"), None, "a prefix is not the package");
        let other = dir.join("data/app/~~z==/com.other.app-a-b==/base.apk");
        std::fs::create_dir_all(other.parent().unwrap()).expect("dirs");
        std::fs::write(&other, b"apk").expect("base.apk");
        assert_eq!(d.installed_packages(), ["com.example.a", "com.other.app"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
