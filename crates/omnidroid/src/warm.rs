//! `omnidroid aosp` on the host's warm device: when one is up (`omnidroid aosp --warm`, or the MCP
//! server's), a session needs no boot of its own. The APK is installed on it (reused when the device
//! holds the same bytes, its data cleared), the account's cookie put in the app's WebView cookie
//! store before its first start, the app started from its launcher, and the place's link sent as
//! soon as the app's main Activity starts -- the app signs in and joins in one start.
//!
//! Measured 2026-10-02 (i7-13700F, Windows, Roblox 2.740.931, PS99): from the command to PS99's own
//! loading screen ~60-75 s with nothing of the app on the device, against ~107 s with the link sent
//! after the sign-in (the app then loads its Home beside the join), and minutes for a session that
//! boots a device of its own.
//!
//! The session ends after `--minutes`, or with this process: the app is stopped and the device
//! stays warm. A helper (`omnidroid warm-release`) stops the app when this process is ended
//! another way (Ctrl+C, a kill).
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use omni_warm as device;

/// The Activity that takes Roblox's place links (`roblox://experiences/start?placeId=`).
const LINK_ACTIVITY: &str = "com.roblox.client.ActivityProtocolLaunch";
/// ActivityTaskManager's line when Roblox's main Activity starts (the splash is done): the
/// earliest moment a place link is taken. One sent before it is lost (2026-10-02: sent when the
/// launcher Activity was displayed, ~1 s earlier, the app never joined until it was sent again).
const MAIN_STARTED: &str = "/.ActivityNativeMain}";
/// A link not answered by "Joining game" in this long is sent again (the app sometimes answers a
/// link by settling on its Home, 2026-09-29), at most `LINK_TRIES` times.
const LINK_RETRY: Duration = Duration::from_secs(60);
const LINK_TRIES: u32 = 4;

/// The warm device a session can use now: up and ready, unless `OMNI_AOSP_WARM=0`, and of the same
/// root profile as the request (`root_hash`: `None` for an unrooted request, which an unrooted
/// device serves as it always did). A device of another profile is not reused.
pub fn usable(root_hash: Option<&str>) -> Option<device::Device> {
    if std::env::var("OMNI_AOSP_WARM").as_deref() == Ok("0") {
        return None;
    }
    let dev = device::find().filter(device::Device::ready)?;
    if dev.root_hash().as_deref() != root_hash {
        println!("[warm] root profile differs: booting a new device");
        return None;
    }
    Some(dev)
}

/// The place's link, as a link opened on a device.
fn link(package: &str, place: u64) -> String {
    format!("am start -a android.intent.action.VIEW -d \"roblox://experiences/start?placeId={place}\" -n {package}/{LINK_ACTIVITY}")
}

/// The guest shell that puts a cookie store (handed over as `store`) into `package`'s WebView data,
/// owned by the app, before its first start.
fn plant(package: &str, store: &str) -> String {
    format!(
        "a=/data/data/{package}; uid=$(stat -c %u $a); d=$a/app_webview/Default; mkdir -p $d && \
         cp {store} $d/Cookies && chown -R $uid:$uid $a/app_webview && chmod 700 $a/app_webview $d && \
         chmod 600 $d/Cookies && echo \"cookie store planted\"; rm -f {store}; "
    )
}

/// A cookie store holding `cookie` (a file), made on the host by `tools/plant_cookie.py --new` --
/// started here, waited for with [`store_made`] (it runs while the APK installs).
fn make_store(repo: &Path, at: &Path, cookie: &Path) -> Result<std::process::Child, String> {
    let python = if cfg!(windows) { "python" } else { "python3" };
    Command::new(python)
        .arg(repo.join("tools/plant_cookie.py"))
        .arg("--new")
        .arg(at)
        .arg(cookie)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{python}: {e}"))
}

/// `make_store`'s end.
fn store_made(child: std::process::Child) -> Result<(), String> {
    let out = child.wait_with_output().map_err(|e| format!("plant_cookie: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("plant_cookie: {}{}", String::from_utf8_lossy(&out.stdout).trim(), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// The session on `dev`: install, plant, start, join; then until `minutes` have passed, the app
/// is stopped or the device is gone.
pub fn session(dev: &device::Device, repo: &Path, apk: &Path, cookie: Option<&Path>, place: Option<u64>, minutes: u64) -> ExitCode {
    let t = Instant::now();
    let say = |what: &str| println!("[warm] +{:.1}s {what}", t.elapsed().as_secs_f64());
    println!("Omnidroid: on the warm device {} (its log: {})", dev.dir.display(), dev.log().display());
    // The cookie store the app's WebView reads at its first start, made while the APK is read.
    // It holds the cookie's value: gone when the session ends, whatever ends it.
    let mut _store_file = None;
    let store = match cookie {
        None => None,
        Some(cookie) => match device::hand_over_path(dev, &format!("cookies-{}.db", std::process::id())).and_then(|(host, guest)| {
            let _ = std::fs::remove_file(&host);
            _store_file = Some(RemovedOnDrop(host.clone()));
            make_store(repo, &host, cookie).map(|child| (child, guest))
        }) {
            Ok(store) => Some(store),
            Err(e) => {
                eprintln!("omnidroid: the cookie store: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
    let app = match device::Apk::read(apk) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("omnidroid: {e}");
            return ExitCode::FAILURE;
        }
    };
    let package = app.package.clone();
    // The cookie, put in the app's data before its first start.
    let mut before = String::new();
    if let Some((child, guest)) = store {
        if let Err(e) = store_made(child) {
            eprintln!("omnidroid: the cookie store: {e}");
            return ExitCode::FAILURE;
        }
        before = plant(&package, &guest);
    }
    let start = match device::launch_command(&app) {
        Ok(start) => start,
        Err(e) => {
            eprintln!("omnidroid: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The device's log from here on: every guest line reaches it.
    let mut log = device::LogTail::from_end(&dev.log());
    // From here the app runs: stopped when this process ends, however it ends.
    release_when_gone(dev, &package);
    // Installed, planted and started in one command.
    let started = match device::install_then(dev, &app, &format!("{before}{start}")) {
        Ok(i) => {
            let gone = if i.uninstalled.is_empty() { String::new() } else { format!("; {} uninstalled", i.uninstalled.join(", ")) };
            say(&format!("{package} {} in {:.1} s{gone}", i.action, i.seconds));
            if i.action == "reused" {
                // The same bytes were installed already: the app's data goes, as a fresh
                // install's would, before it is planted and started.
                device::stop_app(dev, &package).and_then(|_| device::launch(dev, &app, &before))
            } else {
                Ok(i.pm)
            }
        }
        Err(e) => Err(e),
    };
    match started {
        Ok(out) => {
            let planted = if out.contains("cookie store planted") { ", signed in with --cookie (never printed)" } else { "" };
            say(&format!("started{planted}"));
            if cookie.is_some() && planted.is_empty() {
                eprintln!("omnidroid: the cookie store was not planted: {out}");
            }
        }
        Err(e) => {
            eprintln!("omnidroid: {e}");
            return ExitCode::FAILURE;
        }
    }
    let deadline = t + Duration::from_secs(minutes * 60);
    let (mut sent_at, mut tries, mut joined, mut signed_in, mut main_started, mut loaded) = (None::<Instant>, 0u32, false, false, false, false);
    let died = format!("Process {package} (pid ");
    let mut code = ExitCode::SUCCESS;
    while Instant::now() < deadline {
        for line in log.lines() {
            if line.contains(&format!("{package}{MAIN_STARTED}")) && !main_started {
                main_started = true;
            }
            if line.contains("DID_LOG_IN") && !signed_in {
                signed_in = true;
                say("signed in (DID_LOG_IN)");
            }
            if line.contains("! Joining game") && !joined {
                (joined, loaded) = (true, false);
                say("joining the place (Joining game)");
            }
            // Logged by several of the app's parts at once: said once a join.
            if line.contains("onGameLoaded") && !loaded {
                loaded = true;
                say("the place is loaded (onGameLoaded)");
            }
            if line.contains("Upgrade required") {
                eprintln!("omnidroid: the app says \"Upgrade required\": {} is too old for Roblox's servers", apk.display());
            }
            if line.contains(&died) && line.contains("has died") {
                say("the app died: started again");
                let _ = device::launch(dev, &app, "");
                (sent_at, main_started) = (None, false);
            }
        }
        // The place's link: once the main Activity has started (or the app has signed in), again
        // when unanswered.
        if let (Some(place), false) = (place, joined) {
            let due = match sent_at {
                None => main_started || signed_in,
                Some(at) => at.elapsed() > LINK_RETRY && tries < LINK_TRIES,
            };
            if due {
                tries += 1;
                match dev.shell(&link(&package, place), Duration::from_secs(60)) {
                    Ok(_) => say(&format!("place {place}'s link sent{}", if tries > 1 { format!(" (try {tries})") } else { String::new() })),
                    Err(e) => eprintln!("omnidroid: the place's link: {e}"),
                }
                sent_at = Some(Instant::now());
            }
        }
        if !dev.alive() {
            eprintln!("omnidroid: the warm device stopped");
            code = ExitCode::FAILURE;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = dev.shell(&format!("am force-stop {package}"), Duration::from_secs(60));
    say(&format!("{package} stopped; the device stays warm"));
    if place.is_some() && !joined {
        eprintln!("omnidroid: the place was never joined (no \"Joining game\" in {})", dev.log().display());
        code = ExitCode::FAILURE;
    }
    code
}

/// A file removed when this goes out of scope.
struct RemovedOnDrop(std::path::PathBuf);

impl Drop for RemovedOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A helper that stops `package` on `dev` once this process is gone (`omnidroid warm-release`).
fn release_when_gone(dev: &device::Device, package: &str) {
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = Command::new(exe)
        .args(["warm-release", &std::process::id().to_string(), &dev.dir.display().to_string(), package])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// `omnidroid warm-release <pid> <device dir> <package>`: wait for `pid` to end, then stop
/// `package` on the device (a no-op when the session stopped it itself).
pub fn release(args: &[String]) -> ExitCode {
    let [pid, dir, package] = args else { return ExitCode::from(2) };
    let Ok(pid) = pid.parse::<u32>() else { return ExitCode::from(2) };
    let dev = device::Device { dir: dir.into() };
    while omni_platform::process::is_alive(pid) {
        if !dev.alive() {
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let _ = dev.shell(&format!("am force-stop {package}"), Duration::from_secs(60));
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_and_the_plant_name_the_app_and_the_place() {
        assert_eq!(
            link("com.roblox.client", 8737899170),
            "am start -a android.intent.action.VIEW -d \"roblox://experiences/start?placeId=8737899170\" -n com.roblox.client/com.roblox.client.ActivityProtocolLaunch"
        );
        let p = plant("com.roblox.client", "/data/local/tmp/omni-apk/x.Cookies");
        assert!(p.contains("a=/data/data/com.roblox.client;") && p.contains("cp /data/local/tmp/omni-apk/x.Cookies $d/Cookies"));
        assert!(p.ends_with("rm -f /data/local/tmp/omni-apk/x.Cookies; "), "the store handed over is removed, planted or not");
    }
}
