//! What the pool does to one device, through its control channel ([`omni_warm::Device`]): boot it,
//! give an instance a display and an Android user, start and stop the app there, and read what
//! runs where.
//!
//! **An instance is a display and a user.** Its display is added for it (`#host display add`,
//! `omni_linux::displays`: a window titled "Omnidroid" of its own) and enabled; its app runs as a
//! user of its own -- a visible background user, started on that display (`fw.visible_bg_users`),
//! the package installed for it (`pm install-existing`) -- never as user 0, whom Android counts
//! visible on display 0 alone. Closing it stops that user and removes the display; the user is
//! kept for the next instance.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use omni_warm::Device;

use crate::config::Config;

/// How long a shell command on a device may take.
const SHELL: Duration = Duration::from_secs(300);

/// The boot properties a pool device needs: room for many users, and users visible on displays of
/// their own beside user 0.
pub const SETPROPS: &str = "fw.max_users=64,fw.visible_bg_users=1";

/// A run's shell answer, or its failure.
fn run(dev: &Device, command: &str, uid: Option<u32>) -> Result<String, String> {
    let (code, out) = dev.shell_as(command, uid, SHELL)?;
    if code != 0 {
        return Err(format!("`{}` exited {code}: {}", command.lines().next().unwrap_or(command).chars().take(120).collect::<String>(), out.trim()));
    }
    Ok(out)
}

/// As root.
fn root(dev: &Device, command: &str) -> Result<String, String> {
    run(dev, command, Some(0))
}

/// A host command (`omni_linux::displays`): sent with no `#uid` line, so `#host` is the first.
fn host(dev: &Device, command: &str) -> Result<String, String> {
    run(dev, &format!("#host {command}"), None)
}

/// A new device's directory under `root`: `omni-warm-<unix secs>` (a later one if taken), the
/// name `omni_warm::find_all` knows.
#[must_use]
pub fn new_dir(root: &Path) -> PathBuf {
    let mut secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    loop {
        let dir = root.join(format!("{}{secs}", omni_warm::PREFIX));
        if !dir.exists() && !dir.with_extension("ctl").exists() && !dir.with_extension("pool").exists() {
            return dir;
        }
        secs += 1;
    }
}

/// The marker that says a device is the pool's (`<dir>.pool`): an app that opens again adopts
/// the devices it finds with one.
#[must_use]
pub fn marker(dir: &Path) -> PathBuf {
    dir.with_extension("pool")
}

/// Boot a device in `dir`: `omnidroid aosp --warm` (`exe`, this launcher), with every display a
/// window of its own and none for display 0. Its own output goes to `<dir>.launcher.log`.
///
/// # Errors
/// The launcher could not be started.
pub fn boot(exe: &Path, dir: &Path, config: &Config) -> Result<Child, String> {
    let _ = std::fs::write(marker(dir), b"omnidroid app\n");
    let log = std::fs::File::create(dir.with_extension("launcher.log")).map_err(|e| format!("{}: {e}", dir.display()))?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    let setprops = match std::env::var("OMNI_R_SETPROPS") {
        Ok(more) if !more.trim().is_empty() => format!("{SETPROPS},{more}"),
        _ => SETPROPS.to_string(),
    };
    let mut cmd = Command::new(exe);
    // A year: the pool ends the device, not a timer.
    cmd.args(["aosp", "--warm", "--minutes", "525600", "--gpu", &config.gpu, "--instance"]).arg(dir);
    cmd.env("OMNI_MULTI_DISPLAY", "1")
        .env("OMNI_PRIMARY_WINDOW", "0")
        .env("OMNI_SCREENSHOT", dir.with_extension("png"))
        // Written when the MCP `screenshot` asks (`<png>.want`), else once a minute: each is a
        // 3.6 MB PNG a display, and every display wrote one each second for nobody.
        .env("OMNI_SCREENSHOT_MS", "60000")
        .env("OMNI_R_SETPROPS", setprops)
        .env("OMNI_AOSP_WARM", "0");
    // Several apps start on one device, often while another device boots: Android's timeouts scaled
    // as an emulator's are (`ro.hw_timeout_multiplier`, `omni_linux::props`), else an app's start is
    // killed as an ANR ("failed to complete startup") while system_server holds a core (2026-10-10).
    if std::env::var_os("OMNI_HW_TIMEOUT_MULTIPLIER").is_none() {
        cmd.env("OMNI_HW_TIMEOUT_MULTIPLIER", "5");
    }
    // No spare app process (`omni_linux::zygote`'s, ~3-4 s off a cold start): an app whose startup
    // broadcast reaches the spare before its application is bound crashes there ("Unable to
    // instantiate receiver ... Instrumentation.onException on a null object", Super Mechs,
    // 2026-10-10) -- and a pool starts many apps.
    if std::env::var_os("OMNI_APP_SPARE").is_none() {
        cmd.env("OMNI_APP_SPARE", "0");
    }
    if config.device_ram_mb > 0 {
        cmd.env("OMNI_DEVICE_RAM_MB", config.device_ram_mb.to_string());
    }
    cmd.stdin(Stdio::null()).stdout(log).stderr(err);
    cmd.spawn().map_err(|e| format!("{}: {e}", exe.display()))
}

/// The Android display id of the composer's display `hwc`, read from `dumpsys display`: the
/// logical display whose device is named `omnidroid-<hwc>` (`hal::composer::display_name`).
#[must_use]
pub fn android_display(dumpsys: &str, hwc: i64) -> Option<i32> {
    let name = format!("name=omnidroid-{hwc},");
    dumpsys.lines().filter(|l| l.contains("mBaseDisplayInfo=DisplayInfo{") && l.contains(&name)).find_map(|l| {
        let at = l.find(", displayId ")? + ", displayId ".len();
        l[at..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
    })
}

/// A display of `w` x `h` for an instance: added, found in Android, enabled (Android 15 keeps a new
/// external display off until something turns it on), its window's pointer bound to it. Answers
/// the composer's id and Android's.
///
/// # Errors
/// The device's, or Android never showed the display.
pub fn add_display(dev: &Device, (w, h): (u32, u32)) -> Result<(i64, i32), String> {
    let hwc: i64 = host(dev, &format!("display add {w}x{h}"))?.trim().parse().map_err(|_| "display add answered no id".to_string())?;
    let t = Instant::now();
    let android = loop {
        let dump = root(dev, "dumpsys display | grep 'mBaseDisplayInfo=DisplayInfo{'")?;
        if let Some(id) = android_display(&dump, hwc) {
            break id;
        }
        if t.elapsed() > Duration::from_secs(20) {
            let _ = host(dev, &format!("display remove {hwc}"));
            return Err(format!("Android did not show display {hwc} within 20 s"));
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    root(dev, &format!("cmd display enable-display {android}"))?;
    host(dev, &format!("display bind {hwc} {android}"))?;
    Ok((hwc, android))
}

/// Remove the composer's display `hwc` (its window closes).
///
/// # Errors
/// The device's.
pub fn remove_display(dev: &Device, hwc: i64) -> Result<(), String> {
    host(dev, &format!("display remove {hwc}")).map(|_| ())
}

/// The composer's displays beyond display 0 (`display list`).
#[must_use]
pub fn extra_displays(dev: &Device) -> Vec<i64> {
    host(dev, "display list").map(|out| out.lines().filter_map(|l| l.split_whitespace().next()?.parse().ok()).collect()).unwrap_or_default()
}

/// The users there are (`pm list users`): their ids.
#[must_use]
pub fn users(list: &str) -> Vec<u32> {
    list.lines().filter_map(|l| l.trim().strip_prefix("UserInfo{")?.split(':').next()?.parse().ok()).collect()
}

/// The ids of the device's users.
///
/// # Errors
/// The device's.
pub fn list_users(dev: &Device) -> Result<Vec<u32>, String> {
    Ok(users(&root(dev, "pm list users")?))
}

/// A new user: its id.
///
/// # Errors
/// The device's, or `pm create-user` named no id.
pub fn create_user(dev: &Device) -> Result<u32, String> {
    let out = root(dev, "pm create-user omni")?;
    out.rsplit("created user id ").next().and_then(|s| s.trim().split_whitespace().next()?.parse().ok()).ok_or_else(|| format!("pm create-user: {}", out.trim()))
}

/// What grants an app everything it asks for, for `user` -- the device's `omni_autogrant.sh`
/// does it for user 0 only: its runtime permissions, and the special-access app-ops it requests.
#[must_use]
pub fn grant_script(package: &str, user: u32) -> String {
    format!(
        "info=$(dumpsys package {package}); \
         for perm in $(echo \"$info\" | sed -n 's/^ *\\([A-Za-z0-9_.]*\\): granted=false.*/\\1/p' | sort -u); do cmd package grant --user {user} {package} $perm >/dev/null 2>&1; done; \
         for op in MANAGE_EXTERNAL_STORAGE SYSTEM_ALERT_WINDOW REQUEST_INSTALL_PACKAGES WRITE_SETTINGS SCHEDULE_EXACT_ALARM; do \
         case \"$info\" in *\"android.permission.$op\"*) cmd appops set --user {user} {package} $op allow >/dev/null 2>&1;; esac; done; true"
    )
}

/// Install the APK (`omni_warm::install_beside`: replaced when other bytes of it are there).
///
/// # Errors
/// As `install_beside`.
pub fn install(dev: &Device, apk: &omni_warm::Apk) -> Result<omni_warm::Installed, String> {
    omni_warm::install_beside(dev, apk)
}

/// Uninstall a package, for every user.
///
/// # Errors
/// The device's.
pub fn uninstall(dev: &Device, package: &str) -> Result<(), String> {
    root(dev, &format!("pm uninstall {package}")).map(|_| ())
}

/// Start the app as `user` on the Android display `display`: a user other than 0 is made visible
/// there first and has the package installed; `before` (a root shell, e.g. [`crate::extras::plant`])
/// runs then, with the user's data there, and the app is granted what it asks for. Answers
/// `before`'s output and `am start -W`'s.
///
/// # Errors
/// The device's, or the start failed.
pub fn start(dev: &Device, package: &str, launcher: &str, user: u32, display: i32, before: &str) -> Result<String, String> {
    if user != 0 {
        root(dev, &format!("pm install-existing --user {user} {package}"))?;
        root(dev, &format!("am start-user -w --display {display} {user}"))?;
    }
    // Before the grants, so a `before` that resets the app's data leaves them in place.
    let mut out = if before.is_empty() { String::new() } else { root(dev, before)? };
    if user != 0 {
        root(dev, &grant_script(package, user))?;
    }
    let started_out = root(dev, &format!("input keyevent KEYCODE_WAKEUP; am start -W --display {display} --user {user} -n {package}/{launcher}"))?;
    if !started(&started_out) {
        return Err(format!("am start: {}", started_out.trim()));
    }
    out.push_str(&started_out);
    Ok(out)
}

/// Whether `am start -W` says the Activity was displayed: `Status: ok` and `Complete`, no error.
#[must_use]
pub fn started(out: &str) -> bool {
    out.contains("Complete") && !out.contains("Error") && out.lines().filter_map(|l| l.trim().strip_prefix("Status: ")).all(|s| s == "ok")
}

/// Stop the app of an instance: force-stopped for its user, and a user other than 0 stopped (it
/// is kept for the next instance that needs one).
///
/// # Errors
/// The device's.
pub fn stop(dev: &Device, package: &str, user: u32) -> Result<(), String> {
    root(dev, &format!("am force-stop --user {user} {package}"))?;
    if user != 0 {
        root(dev, &format!("am stop-user -w {user}"))?;
    }
    Ok(())
}

/// What runs on each Android display: `(display, user, package)` of every activity, from
/// `dumpsys activity activities`.
#[must_use]
pub fn activities(dump: &str) -> Vec<(i32, u32, String)> {
    let mut out = Vec::new();
    let mut display = None;
    for line in dump.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Display #") {
            display = rest.split(|c: char| !c.is_ascii_digit()).next().and_then(|d| d.parse().ok());
            continue;
        }
        let Some(d) = display else { continue };
        // `ActivityRecord{<hash> u<user> <package>/<class> t<task>}`
        for rec in t.split("ActivityRecord{").skip(1) {
            let mut words = rec.split_whitespace();
            let _hash = words.next();
            let (Some(user), Some(component)) = (words.next(), words.next()) else { continue };
            let (Some(user), Some(package)) = (user.strip_prefix('u').and_then(|u| u.parse().ok()), component.split('/').next()) else { continue };
            if !out.contains(&(d, user, package.to_string())) {
                out.push((d, user, package.to_string()));
            }
        }
    }
    out
}

/// What runs where now.
///
/// # Errors
/// The device's.
/// The app processes running on the device, as its host knows them (`#host apps`, no guest
/// process started): (Android user, process name).
pub fn apps(dev: &Device) -> Result<Vec<(u32, String)>, String> {
    Ok(host(dev, "apps")?
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let (_pid, uid, name) = (w.next()?, w.next()?.parse::<u32>().ok()?, w.next()?);
            Some((uid / 100_000, name.to_string()))
        })
        .collect())
}

pub fn running(dev: &Device) -> Result<Vec<(i32, u32, String)>, String> {
    Ok(activities(&root(dev, "dumpsys activity activities | grep -E 'Display #|ActivityRecord'")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_names_the_display_it_made_of_ours() {
        let dump = "    mBaseDisplayInfo=DisplayInfo{\"Built-in Screen\", displayId 0, displayGroupId 0, address {port=0}, deviceProductInfo DeviceProductInfo{name=omnidroid, manufacturerPnpId=OMD}\n\
                    mBaseDisplayInfo=DisplayInfo{\"HDMI Screen\", displayId 3, displayGroupId 0, address {port=2}, deviceProductInfo DeviceProductInfo{name=omnidroid-2, manufacturerPnpId=OMD}\n\
                    mBaseDisplayInfo=DisplayInfo{\"HDMI Screen\", displayId 12, displayGroupId 0, deviceProductInfo DeviceProductInfo{name=omnidroid-12, manufacturerPnpId=OMD}\n";
        assert_eq!(android_display(dump, 2), Some(3));
        assert_eq!(android_display(dump, 12), Some(12), "omnidroid-1 is not omnidroid-12");
        assert_eq!(android_display(dump, 1), None);
    }

    #[test]
    fn users_and_activities_are_read_from_their_dumps() {
        assert_eq!(users("Users:\n\tUserInfo{0:Owner:4c13} running\n\tUserInfo{10:omni:410}\n"), [0, 10]);
        let dump = "Display #2 (activities from top to bottom):\n    topResumedActivity=ActivityRecord{152fd2a u0 com.a/.Main t3}\n      * Hist  #0: ActivityRecord{152fd2a u0 com.a/.Main t3}\n\
                    Display #3 (activities from top to bottom):\n    topResumedActivity=ActivityRecord{13cb733 u10 com.a/.Main t4}\n\
                    Display #0 (activities from top to bottom):\n      topResumedActivity=ActivityRecord{8fd8ba4 u0 com.android.settings/.FallbackHome t2}\n";
        assert_eq!(activities(dump), [(2, 0, "com.a".to_string()), (3, 10, "com.a".to_string()), (0, 0, "com.android.settings".to_string())]);
    }

    #[test]
    fn a_new_device_s_directory_is_numbered_as_omni_warm_finds_it() {
        let root = std::env::temp_dir().join(format!("omni-pool-dirs-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let a = new_dir(&root);
        std::fs::create_dir_all(&a).unwrap();
        let b = new_dir(&root);
        assert_ne!(a, b);
        assert!(omni_warm::numbered(&a.file_name().unwrap().to_string_lossy(), omni_warm::PREFIX));
        assert_eq!(marker(&a), a.with_extension("pool"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_start_counts_only_when_android_displayed_it() {
        assert!(started("Starting: Intent { cmp=a/.M }\nStatus: ok\nLaunchState: COLD\nTotalTime: 7416\nWaitTime: 7513\nComplete\n"));
        assert!(!started("Starting: Intent { cmp=a/.M }\nStatus: timeout\nComplete\n"), "never displayed");
        assert!(!started("Error type 3\nError: Activity class {a/a.M} does not exist.\n"));
        assert!(!started("Starting: Intent { cmp=a/.M }\n"), "no answer");
    }

    #[test]
    fn the_grant_names_the_user() {
        let s = grant_script("com.a", 11);
        assert!(s.contains("cmd package grant --user 11 com.a") && s.contains("cmd appops set --user 11 com.a"));
    }
}
