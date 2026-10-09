//! R: the owner's APK (`OMNI_TEST_APK`, else the stock `Roblox-2.738.1397.apk` in the repository
//! root) installed with `pm install` and started from its launcher on the real AOSP stack -- as a
//! device installs and starts it -- until its own UI is on the host display.
//!
//! The session is kept for the run's reader: every line in `<temp>/omni-linux-r-<pid>.log`, and the
//! display every 20 s in `<temp>/omni-linux-r-<pid>-shots/NNNN.png` (`tools/shot_stats.py` gives
//! their colour histograms). `OMNI_R_MINUTES` bounds the session (default 20); `OMNI_R_THEN`
//! is shell run after the launch (a deep link, a cookie); `OMNI_R_EXPECT` names the lines that must
//! have appeared, `|`-separated (default: the app's process started and a frame presented).
//! The app runs on a device without SystemUI or a launcher in its image (`OMNI_R_KIOSK=0`: with
//! them): no bars, no taskbar, the whole display the app's, from the first boot. The composer presents only the app either way
//! (`hal::composer`, `OMNI_APP_ONLY`).
//!
//! Minutes long: `cargo test -p omni-linux --release --test r_roblox -- --ignored --nocapture`.
mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Share of near-black pixels and number of distinct colours (5 bits a channel) in the runner's
/// screenshot, if one is there and whole.
fn histogram(png: &Path) -> Option<(f64, usize)> {
    let d = std::fs::read(png).ok()?;
    let (mut at, mut width, mut height, mut idat) = (8, 0usize, 0usize, Vec::new());
    while at + 8 <= d.len() {
        let n = u32::from_be_bytes(d[at..at + 4].try_into().ok()?) as usize;
        let kind = &d[at + 4..at + 8];
        let body = d.get(at + 8..at + 8 + n)?;
        if kind == b"IHDR" {
            width = u32::from_be_bytes(body[0..4].try_into().ok()?) as usize;
            height = u32::from_be_bytes(body[4..8].try_into().ok()?) as usize;
        } else if kind == b"IDAT" {
            idat.extend_from_slice(body);
        }
        at += 12 + n;
    }
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(&idat[..]).read_to_end(&mut raw).ok()?;
    let (mut black, mut colours) = (0usize, std::collections::HashSet::new());
    for y in 0..height {
        let row = raw.get(y * (width * 4 + 1) + 1..(y + 1) * (width * 4 + 1))?;
        for p in row.chunks_exact(4) {
            if p[0] < 16 && p[1] < 16 && p[2] < 16 {
                black += 1;
            }
            colours.insert((p[0] >> 3, p[1] >> 3, p[2] >> 3));
        }
    }
    Some((black as f64 / (width * height).max(1) as f64, colours.len()))
}


/// What belongs to one boot or one run and is no part of a saved device: the bind table (vold
/// binds again at boot), the loopback namespace, and the run's own files in `/data/local/tmp`.
const NOT_SAVED: &[&str] = &[".omni-binds", ".omni-loopback", "data/local/tmp"];

/// Copy the device at `from` into `to` (made), less `NOT_SAVED`: the bytes copied.
fn copy_device(from: &Path, to: &Path) -> std::io::Result<u64> {
    fn walk(from: &Path, to: &Path, rel: &Path, bytes: &mut u64) -> std::io::Result<()> {
        std::fs::create_dir_all(to.join(rel))?;
        for e in std::fs::read_dir(from.join(rel))? {
            let e = e?;
            let r = rel.join(e.file_name());
            if NOT_SAVED.iter().any(|n| r == Path::new(n)) {
                continue;
            }
            if e.file_type()?.is_dir() {
                walk(from, to, &r, bytes)?;
            } else {
                *bytes += std::fs::copy(e.path(), to.join(&r))?;
            }
        }
        Ok(())
    }
    let mut bytes = 0;
    walk(from, to, Path::new(""), &mut bytes)?;
    std::fs::create_dir_all(to.join("data/local/tmp"))?;
    Ok(bytes)
}

/// Save the device at `kept` (its boot ended) as `g`, whole or not at all: copied beside, then
/// renamed; `ready` says what it was saved from. The copy's per-boot files are then removed from
/// `kept`, which boots again.
fn save_device(kept: &Path, g: &Path, from: &str) {
    let t = Instant::now();
    let part = g.with_extension(format!("part-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&part);
    match copy_device(kept, &part.join("device")) {
        Ok(bytes) => {
            let _ = std::fs::write(part.join("ready"), format!("{from}
"));
            let _ = std::fs::remove_dir_all(g);
            let _ = std::fs::create_dir_all(g.parent().unwrap_or(Path::new(".")));
            match std::fs::rename(&part, g) {
                Ok(()) => eprintln!("[r] device saved as {}: {} MiB in {} ms", g.display(), bytes >> 20, t.elapsed().as_millis()),
                Err(e) => eprintln!("[r] device not saved as {}: {e}", g.display()),
            }
        }
        Err(e) => eprintln!("[r] device not saved: {e}"),
    }
    let _ = std::fs::remove_dir_all(&part);
    for f in NOT_SAVED.iter().filter(|f| f.starts_with(".omni")) {
        let _ = std::fs::remove_dir_all(kept.join(f));
        let _ = std::fs::remove_file(kept.join(f));
    }
}

// A test device trimmed as test images are (`OMNI_R_LEAN=0` keeps everything): the image's apps
// that nothing here uses are disabled, so they neither run nor come back -- each app host
// process holds ~263 MiB, and with ~20 of them started at boot the host ran out of memory as
// the game loaded (r11: two sessions stopped by Claude Code's low-memory reaper).
// What the device has no use for at all is not in its image (`omni_linux::device::LEAVES_OUT`:
// never started, not even at boot or when persistent), and ActivityManager keeps no cached app
// process (`omni_lean.sh`); these are the image's apps it still has, disabled once it is up.
const IDLE_APPS: &[&str] = &[
    "com.android.cellbroadcastreceiver", "com.android.cellbroadcastreceiver.module", "com.android.nfc",
    "com.android.healthconnect.controller", "com.android.ondevicepersonalization.services",
    "com.android.devicelockcontroller", "com.android.statementservice", "com.android.documentsui",
    "com.android.federatedcompute.services", "com.android.adservices.api", "com.android.managedprovisioning",
    "com.android.rkpdapp", "com.android.externalstorage", "com.android.keychain",
    "com.android.ext.adservices.api", "com.android.providers.userdictionary", "com.android.wallpaperbackup",
    "com.android.cellbroadcastservice",
];

/// The script that disables `IDLE_APPS` once the device is up (`OMNI_R_LEAN=0`: none) -- in the
/// background with `OMNI_R_FAST_SETUP=1`, joined after the install (`common::r_scripts`).
fn lean_script() -> String {
    // `OMNI_DEVICE_IDLE_APPS=out`: they are not in the image (`omni_linux::device::IDLE_APPS_LEFT_OUT`),
    // so there is nothing to disable.
    if std::env::var("OMNI_R_LEAN").as_deref() == Ok("0") || omni_linux::device::idle_apps_out() {
        return String::new();
    }
    common::r_scripts::lean(IDLE_APPS, common::r_scripts::fast_setup())
}

/// The device is set up as a freely resizable one is: Developer options' "Force activities to be
/// resizable", for every app (the owner's choice, 2026-09-28; `OMNI_R_RESIZABLE=0` leaves it off).
/// The APK declares `resizeableActivity="false"` on its application, and without this Android
/// answers a display resize with size-compatibility mode -- the app kept at its old size, scaled,
/// and a "restart for a better view" button (run 2026-09-28, 1280x720 -> 817x542) -- rather than a
/// new size for the app to draw at.
fn resizable_script() -> &'static str {
    if std::env::var("OMNI_R_RESIZABLE").as_deref() != Ok("0") {
        "settings put global force_resizable_activities 1; echo \"[r] activities resizable\"; "
    } else {
        ""
    }
}

/// Until Android says it has booted (`sys.boot_completed`), at most 20 minutes.
const BOOTED: &str = "i=0; until [ \"$(getprop sys.boot_completed)\" = 1 ] || [ $i -ge 1200 ]; do sleep 1; i=$((i+1)); done; \
                      echo \"[r] boot_completed=$(getprop sys.boot_completed)\"; ";

/// A new device's settings, as its owner would set them once: set up, the screen kept on, no
/// animations, resizable activities, the idle apps disabled.
fn settings_script() -> String {
    format!(
        "settings put global device_provisioned 1; settings put secure user_setup_complete 1; \
         settings put system screen_off_timeout 1800000; {}\
         input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard; \
         settings put global window_animation_scale 0; settings put global transition_animation_scale 0; \
         settings put global animator_duration_scale 0; settings put secure immersive_mode_confirmations confirmed; \
         {}{}",
        common::r_scripts::stay_on(common::r_scripts::fast_setup()),
        resizable_script(),
        lean_script()
    )
}

/// How saved devices are set up: a device saved by an older setup is not booted (2: the package
/// installer kept enabled).
const DEVICE_SETUP: u32 = 2;

/// The saved device for this APK, account and device setup under `root`: a new APK, a new cookie
/// file or another setup is another device.
fn golden_dir(root: &Path, apk: &Path, cookie: Option<&Path>, kiosk: bool, locale: &str) -> PathBuf {
    golden_dir_rooted(root, apk, cookie, kiosk, locale, None)
}

/// `golden_dir` for a rooted device: `-root-<hash>` (`root::key::root_hash`) is appended, so a
/// rooted device is never an unrooted one, nor one with other modules. `None`: the unrooted key,
/// unchanged.
fn golden_dir_rooted(root: &Path, apk: &Path, cookie: Option<&Path>, kiosk: bool, locale: &str, root_hash: Option<&str>) -> PathBuf {
    let stamp = |p: &Path| {
        std::fs::metadata(p).map_or((0, 0), |m| {
            let at = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            (m.len(), at)
        })
    };
    let stem = |p: &Path| p.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let (apk_len, _) = stamp(apk);
    let account = cookie.map_or_else(|| "guest".to_string(), |c| format!("{}-{}", stem(c), stamp(c).1));
    let rooted = root_hash.map_or_else(String::new, |h| format!("-root-{h}"));
    // `-bootu`: a device made with the uncompressed boot image (`omni_linux::boot_image`) is not one
    // made without it.
    let bootu = omni_linux::boot_image::key_suffix();
    // `-idleout`: a device whose image has no idle apps (`OMNI_DEVICE_IDLE_APPS=out`) is another.
    let idle = omni_linux::device::idle_apps_key_suffix();
    let name = format!("{}-{apk_len}-{account}-{}-{locale}-v{DEVICE_SETUP}{rooted}{bootu}{idle}", stem(apk), if kiosk { "kiosk" } else { "ui" });
    root.join(name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect::<String>())
}

/// The repository this test was built from.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).expect("crates/omni-linux has two ancestors").to_path_buf()
}

/// The rooted device `OMNI_R_ROOT` / `OMNI_R_MODULES` / `OMNI_R_SU` ask for, if any. Missing Magisk
/// assets (or an unknown module) stop the run: a half-rooted device is never booted.
fn root_request() -> Option<omni_linux::root::key::Request> {
    match omni_linux::root::key::from_env(&repo_root()) {
        Ok(request) => request,
        Err(e) => panic!("the rooted device cannot be made: {e} (the Magisk assets: run `python tools/fetch_magisk.py`)"),
    }
}

/// Stage `instance` as `request` says, before the device boots (a fresh device only: a saved or
/// warm device of the same root hash was staged when it was made, and staging again would delete and
/// recopy the modules, losing what they wrote since).
fn stage_root(instance: &Path, request: &omni_linux::root::key::Request) {
    omni_linux::root::install::stage(instance, &request.profile, &request.catalog, &request.assets).unwrap_or_else(|e| panic!("staging the root: {e}"));
    eprintln!("[r] rooted device staged (root profile {})", request.hash);
}

/// The place's deep link, as a link opened on a device, up to four times (the app sometimes
/// answers the link by restarting its session and settling on Home instead of joining,
/// 2026-09-29); `first` seconds for the first try to show "Joining game" (a cold app signs in
/// first), 90 for the others. An app that dies on the way (`app-died`, written by the host) is
/// started again at once: an app started while the device is still busy with its boot can lose
/// libzstd-jni's startup race (its worker calls through a table ~20 s after load, and the
/// initialisation that fills it came later -- SIGSEGV at 0x40, 2026-09-30). `id` is a number or a
/// shell variable. `joining` is written when the app logs "Joining game".
fn join_script(id: &str, first: u32) -> String {
    format!(
        "rm -f /data/local/tmp/joining /data/local/tmp/app-died; for try in 1 2 3 4; do \
         am start -a android.intent.action.VIEW -d \"roblox://experiences/start?placeId={id}\" -n com.roblox.client/com.roblox.client.ActivityProtocolLaunch; \
         echo \"[r] join intent for {id} (try $try): $?\"; \
         w=90; [ $try = 1 ] && w={first}; \
         j=0; until [ -e /data/local/tmp/joining ] || [ -e /data/local/tmp/app-died ] || [ $j -ge $w ]; do sleep 1; j=$((j+1)); done; \
         [ -e /data/local/tmp/joining ] && break; \
         if [ -e /data/local/tmp/app-died ]; then rm -f /data/local/tmp/app-died; echo \"[r] the app died: started again\"; fi; done; "
    )
}

#[test]
#[ignore = "boots the whole system, installs and starts an app: many minutes"]
fn the_apk_is_installed_started_and_draws() {
    let Some(sysroot) = common::sysroot() else { return };
    if std::env::var("OMNI_R_WARM").as_deref() == Ok("1") {
        return warm_device(&sysroot);
    }
    let apk = std::env::var_os("OMNI_TEST_APK")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Roblox-2.738.1397.apk"), PathBuf::from);
    assert!(apk.exists(), "no APK at {}", apk.display());
    let minutes: u64 = std::env::var("OMNI_R_MINUTES").ok().and_then(|m| m.parse().ok()).unwrap_or(20);
    // `OMNI_R_INSTANCE=<dir>`: the instance's directory, named by the caller (the MCP server, which
    // reaches a standby instance through its files); else `<temp>/omni-linux-r-<pid>`.
    let instance = std::env::var_os("OMNI_R_INSTANCE")
        .map_or_else(|| std::env::temp_dir().join(format!("omni-linux-r-{}", std::process::id())), PathBuf::from);
    let _ = std::fs::remove_dir_all(&instance);
    let tmp = instance.join("data/local/tmp");
    // A caller (the MCP server, `omni-mcp`) can name the live framebuffer PNG up front by presetting
    // OMNI_SCREENSHOT, so it knows where to read a frame from without guessing this process's pid.
    // Otherwise the default is this instance's own path.
    let screenshot = std::env::var_os("OMNI_SCREENSHOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| instance.with_extension("png"));
    let shots = PathBuf::from(format!("{}-shots", instance.display()));
    let _ = std::fs::create_dir_all(&shots);
    std::env::set_var("OMNI_SCREENSHOT", &screenshot);

    let extra = std::env::var("OMNI_R_THEN").unwrap_or_default();
    // `OMNI_R_COOKIE=<file>`: the app's first start makes its own cookie store; once the host sees
    // it (`cookie-store`, below: a fixed 45 s was too short where the first start is slow -- Linux,
    // 4 cores), the app is stopped, the session cookie put in that store (`tools/plant_cookie.py`,
    // never printed), and the app started again -- as a device that had signed in starts it.
    let cookie = std::env::var_os("OMNI_R_COOKIE").map(PathBuf::from);
    // `OMNI_R_PLANT_FIRST=1`: the store made on the host now and put in the app's data before its
    // first start (`common::r_scripts::plant`), instead of the dance below.
    let plant_first = cookie.is_some() && common::r_scripts::plant_first();
    let sign_in = if cookie.is_some() && !plant_first {
        "i=0; until [ -e /data/local/tmp/cookie-store ] || [ $i -ge 600 ]; do sleep 1; i=$((i+1)); done; sleep 10; \
         am force-stop com.roblox.client; echo \"[r] cookie-stop\"; \
         i=0; until [ -e /data/local/tmp/cookie-planted ] || [ $i -ge 180 ]; do sleep 1; i=$((i+1)); done; \
         echo \"[r] cookie planted: $(cat /data/local/tmp/cookie-planted)\"; \
         am start -W -n \"$act\"; echo \"[r] relaunched: $?\"; "
    } else {
        ""
    };
    let place = std::env::var("OMNI_R_PLACE").ok();
    // `OMNI_R_PLACE=<id>`: once signed in, the place's deep link, as a link opened on a device.
    let join = place.as_deref().map_or_else(String::new, |id| {
        format!(
            "i=0; until [ -e /data/local/tmp/signed-in ] || [ $i -ge 900 ]; do sleep 1; i=$((i+1)); done; sleep {}; {}",
            common::r_scripts::link_delay(),
            join_script(id, 90)
        )
    });
    // `OMNI_R_STANDBY=1`: once launched (and in the place, with `OMNI_R_PLACE`), the device waits
    // for the host: a place id written to `/data/local/tmp/join-place` is joined (the MCP server's
    // `start_instance` on a standby instance). The host writes `/data/local/tmp/standby` when the
    // wait starts, `game-loaded` (the place id) while a place is loaded, and rejoins a place the
    // server disconnected; `/data/local/tmp/stop` ends the run.
    let standby = std::env::var("OMNI_R_STANDBY").as_deref() == Ok("1");
    let wait = if standby {
        format!(
            "echo \"[r] standby ready\"; \
             while :; do if [ -e /data/local/tmp/join-place ]; then id=$(cat /data/local/tmp/join-place); rm -f /data/local/tmp/join-place; \
             echo \"[r] standby join $id\"; {} fi; sleep 1; done; ",
            join_script("$id", 90)
        )
    } else {
        String::new()
    };
    let lean = lean_script();
    // A dedicated single-app device (the default; `OMNI_R_KIOSK=0`: with SystemUI and the
    // launcher, ~1.1 GiB more, run 2026-09-28) -- no SystemUI and no launcher in its image
    // (`omni_linux::device::KIOSK_LEAVES_OUT`), so no status bar, navigation bar, taskbar or
    // keyguard, and the app is given the whole display, from the first boot. (Disabling SystemUI
    // on a running device locks it: Android shows the keyguard when the keyguard's service,
    // SystemUI's, dies -- `tests/d8_app_only.rs` disables it and boots again.)
    let kiosk = std::env::var("OMNI_R_KIOSK").as_deref() != Ok("0");
    if kiosk {
        std::env::set_var("OMNI_DEVICE_APPS", "kiosk");
    }
    // After the install: the package installer, which the install itself needed. (A device that
    // boots again must keep it: PackageManager does not start without an installer, "There must
    // be exactly one installer; found []".)
    let lean_after = if lean.is_empty() { String::new() } else { "cmd package disable-user --user 0 com.android.packageinstaller >/dev/null 2>&1; ".to_string() };
    // `OMNI_R_AFTER_INSTALL`: shell run once the APK is installed, before its first start (an
    // app-op the APK asks for, granted as its owner would grant it in Settings).
    let after_install = std::env::var("OMNI_R_AFTER_INSTALL").map_or_else(|_| String::new(), |c| format!("{c}; echo \"[r] after install: $?\"; "));
    let booted = BOOTED;
    // A device to be saved keeps its package installer (PackageManager does not start without one):
    // each boot of the saved device disables it once up.
    // `OMNI_R_INSTALLER`: the installer package recorded for the APK (`pm install -i <pkg>`), so
    // `getInstallerPackageName`/`InstallSourceInfo` report it (e.g. `com.android.vending` for an
    // app whose anti-tamper checks it was installed from the Play Store).
    let installer = std::env::var("OMNI_R_INSTALLER").map_or_else(|_| String::new(), |p| format!(" -i {p}"));
    let setup = |lean_after: &str| {
        format!(
            "{}pm install -r -g{installer} /data/local/tmp/app.apk; echo \"[r] pm install: $?\"; {}{lean_after}{after_install}",
            settings_script(),
            common::r_scripts::lean_wait(common::r_scripts::fast_setup() && !lean.is_empty())
        )
    };
    let resolve = "pkg=$(pm list packages -3 | head -1 | sed 's/^package://'); echo \"[r] package $pkg\"; \
                   act=$(cmd package resolve-activity --brief -c android.intent.category.LAUNCHER \"$pkg\" | tail -1); echo \"[r] launcher $act\"; ";
    // The device's language: the account's (`OMNI_R_LOCALE`, default tr-TR -- the owner's accounts
    // are Turkish). The app applies the account's locale to itself once signed in; on a device in
    // another language that is a configuration change ActivityNativeMain does not handle
    // (configChanges 0xfb0 leaves locale out), and the relaunch ended the game session the deep link
    // had just started (r12: "Updating App configuration based on locale tr_tr", then "Schedule
    // relaunch activity", "Ending game session with place ID 8737899170").
    let locale_name = std::env::var("OMNI_R_LOCALE").unwrap_or_else(|_| "tr-TR".into());
    let locale = format!("persist.sys.locale={locale_name}");
    let boot_args = boot_args(&locale);
    let boot_args: Vec<&str> = boot_args.iter().map(String::as_str).collect();

    // `OMNI_R_GOLDEN=<dir>`: devices are saved there and booted again, not made anew. The first run
    // for an APK, account and setup boots a new device, installs the APK and signs in as above; once
    // signed in, the app is stopped, the device shut down and its directory saved (`golden_dir`),
    // and the same device boots again. Every later run starts from a copy of the saved one: an
    // installed, compiled, signed-in device -- no first boot (the APEXes decompressed, the packages
    // scanned, the roles granted), no install, no cookie planted -- and opens the place's link at
    // once, the app starting cold on it. A saved device whose app never signs in is set aside.
    let root_req = root_request();
    let golden = std::env::var_os("OMNI_R_GOLDEN")
        .map(|root| golden_dir_rooted(Path::new(&root), &apk, cookie.as_deref(), kiosk, &locale_name, root_req.as_ref().map(|r| r.hash.as_str())));
    let warm = golden.as_ref().is_some_and(|g| g.join("ready").exists());
    let saving = !warm && golden.is_some();
    // The script of a saved device's boot: the place at once, or the app's launcher.
    // The app is started from its launcher, as the first boot starts it, and the place's link sent
    // once it has signed in: a link that starts the app cold takes the slower way in
    // (ActivityProtocolLaunch finds no settings, finishes, waits out a pause timeout, then starts the
    // splash), which put libzstd-jni's initialisation past its worker's ~20 s -- 5 of 6 such starts
    // died (2026-09-30), against none from the launcher. An app that dies on the way is started
    // again.
    let warm_then = {
        let start = "am start -W -n \"$act\"; echo \"[r] am start: $?\"; ";
        let open = place.as_deref().map_or_else(String::new, |id| {
            format!(
                "i=0; until [ -e /data/local/tmp/signed-in ] || [ $i -ge 300 ]; do \
                 if [ -e /data/local/tmp/app-died ]; then rm -f /data/local/tmp/app-died; am start -n \"$act\"; echo \"[r] the app died: started again\"; fi; \
                 sleep 1; i=$((i+1)); done; {}",
                join_script(id, 120)
            )
        });
        format!("{booted}{lean_after}input keyevent KEYCODE_WAKEUP; {resolve}rm -f /data/local/tmp/app-died; {start}{open}{wait}{extra}")
    };
    let then = if warm {
        let g = golden.as_ref().expect("a saved device");
        let t = Instant::now();
        let bytes = copy_device(&g.join("device"), &instance).expect("the saved device copied");
        eprintln!("[r] device from {}: {} MiB in {} ms", g.display(), bytes >> 20, t.elapsed().as_millis());
        warm_then.clone()
    } else {
        std::fs::create_dir_all(&tmp).expect("/data/local/tmp");
        std::fs::copy(&apk, tmp.join("app.apk")).expect("the APK");
        if plant_first {
            // The app's cookie store, made as its WebView makes it, holding the account's cookie.
            let tool = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/plant_cookie.py");
            let python = if cfg!(windows) { "python" } else { "python3" };
            let made = std::process::Command::new(python)
                .arg(&tool)
                .arg("--new")
                .arg(tmp.join("cookies.db"))
                .arg(cookie.as_deref().expect("a cookie file"))
                .output()
                .expect("python, for tools/plant_cookie.py");
            assert!(made.status.success(), "the cookie store: {}", String::from_utf8_lossy(&made.stderr).trim());
            eprintln!("[r] cookie store made, to be planted before the first start");
        }
        if let Some(request) = &root_req {
            stage_root(&instance, request);
        }
        // A device to be saved: signed in, the app given time to write what its first start
        // writes, stopped, and the disk synced -- then the host saves it (below).
        // With no account, the app's first start (its screen up, 30 s) is what is saved.
        let rest = if saving {
            let settled = if cookie.is_some() {
                "i=0; until [ -e /data/local/tmp/signed-in ] || [ $i -ge 900 ]; do sleep 1; i=$((i+1)); done; sleep 15; "
            } else {
                "sleep 30; "
            };
            format!("{settled}am force-stop \"$pkg\"; sync; sleep 2; echo \"[r] device quiet\"; ")
        } else {
            format!("{join}{wait}{extra}")
        };
        let setup = setup(if saving { "" } else { &lean_after });
        let planted = if plant_first { common::r_scripts::plant("/data/local/tmp/cookies.db") } else { String::new() };
        format!("{booted}{setup}{resolve}{planted}am start -W -n \"$act\"; echo \"[r] am start: $?\"; {sign_in}{rest}")
    };
    let kept = instance.clone();
    let mut boot = common::boot::Boot::start(&sysroot, instance, &boot_args, &then);
    let expect: Vec<String> = std::env::var("OMNI_R_EXPECT")
        .unwrap_or_else(|_| "[zygote] launching com.roblox.client|frames presented".into())
        .split('|')
        .map(String::from)
        .collect();
    let mut seen = vec![false; expect.len()];
    let started = Instant::now();
    let deadline = started + Duration::from_secs(minutes * 60);
    let mut last_shot = Instant::now();
    let mut n = 0;
    let mut kicked = false;
    let mut joined = false;
    let mut signed_in = false;
    let mut app_died = false;
    let mut loaded_place: Option<String> = None;
    let quiet = std::cell::Cell::new(false);
    let stopped = std::cell::Cell::new(false);
    let in_tmp = |name: &str| kept.join("data/local/tmp").join(name);
    // `OMNI_R_SHOT_SECS`: how often the display is kept (default 20).
    let shot_every = Duration::from_secs(std::env::var("OMNI_R_SHOT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(20));
    let mut on_line = |line: &str| -> bool {
        for (s, e) in seen.iter_mut().zip(&expect) {
            *s |= line.contains(e.as_str());
        }
        let store = kept.join("data/data/com.roblox.client/app_webview/Default/Cookies");
        if cookie.is_some() && !warm && !quiet.get() && !in_tmp("cookie-store").exists() && store.exists() {
            let _ = std::fs::write(in_tmp("cookie-store"), "1");
            eprintln!("[r] +{}s the app's cookie store is there", started.elapsed().as_secs());
        }
        if line.contains("[r] cookie-stop") {
            if let Some(file) = &cookie {
                std::thread::sleep(Duration::from_secs(3));
                let tool = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/plant_cookie.py");
                // Windows installs `python`; macOS and most Linux hosts have no `python`, only
                // `python3` (Ubuntu ships no `python` at all; MEASURED on the M1). On Windows,
                // `python3` can be the Store's stub, so prefer `python` there.
                let python = if cfg!(windows) { "python" } else { "python3" };
                let planted = std::process::Command::new(python).arg(&tool).arg(&store).arg(file).output();
                let said = match &planted {
                    Ok(o) => format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
                    Err(e) => format!("python: {e}"),
                };
                eprintln!("[r] {}", said.trim());
                let ok = planted.is_ok_and(|o| o.status.success());
                let _ = std::fs::write(in_tmp("cookie-planted"), if ok { "ok" } else { "failed" });
            }
        }
        // The server's kick: the display as it was when the line came (the game's last frame, before
        // the app draws its dialog), and the periodic shot before that. A standby device joins the
        // place again.
        if line.contains("Client has been disconnected") {
            if !kicked {
                kicked = true;
                let _ = std::fs::copy(&screenshot, shots.join("kick.png"));
                if n > 0 {
                    let _ = std::fs::copy(shots.join(format!("{:04}.png", n - 1)), shots.join("kick-before.png"));
                }
                eprintln!("[r] +{}s kick: display saved as {}", started.elapsed().as_secs(), shots.join("kick.png").display());
            }
            let _ = std::fs::remove_file(in_tmp("game-loaded"));
            if let (true, Some(id)) = (standby, loaded_place.take()) {
                eprintln!("[r] +{}s disconnected from {id}: joining again", started.elapsed().as_secs());
                let _ = std::fs::write(in_tmp("join-place"), &id);
            }
        }
        if line.contains("DID_LOG_IN") {
            signed_in = true;
            if !in_tmp("signed-in").exists() {
                let _ = std::fs::write(in_tmp("signed-in"), "1");
            }
        }
        // The app's process died (a crash, a kill): the join script starts it again.
        // A standby device that was in a place joins it again.
        if line.contains("Process com.roblox.client (pid ") && line.contains("has died") {
            let _ = std::fs::write(in_tmp("app-died"), "1");
            app_died = true;
            let _ = std::fs::remove_file(in_tmp("game-loaded"));
            if let (true, Some(id)) = (standby, loaded_place.take()) {
                eprintln!("[r] +{}s the app died in {id}: joining again", started.elapsed().as_secs());
                let _ = std::fs::write(in_tmp("join-place"), &id);
            }
        }
        // A game joined is an account signed in (a saved device opening a place logs no DID_LOG_IN).
        if line.contains("Joining game") {
            joined = true;
            signed_in = true;
            if !in_tmp("joining").exists() {
                let _ = std::fs::write(in_tmp("joining"), "1");
            }
        }
        if let Some(at) = line.find("onGameLoaded() SessionReporterState_GameLoaded placeId:") {
            let id: String = line[at..].chars().skip_while(|c| *c != ':').skip(1).take_while(char::is_ascii_digit).collect();
            let _ = std::fs::write(in_tmp("game-loaded"), &id);
            loaded_place = Some(id);
        }
        if line.contains("[r] standby ready") {
            let _ = std::fs::write(in_tmp("standby"), "1");
        }
        // A saved device whose app has run 10 minutes without signing in (a cookie Roblox has since
        // ended) is set aside: the next run makes a new one. An app that keeps dying (an emulation
        // fault) says nothing about the account, and does not.
        if warm && !signed_in && !app_died && started.elapsed() > Duration::from_secs(600) {
            if let Some(g) = golden.as_ref().filter(|g| g.join("ready").exists()) {
                eprintln!("[r] the saved device {} never signed in: set aside", g.display());
                let _ = std::fs::remove_file(g.join("ready"));
            }
        }
        if line.contains("[r] device quiet") {
            quiet.set(true);
            return true;
        }
        if in_tmp("stop").exists() {
            stopped.set(true);
            return true;
        }
        if last_shot.elapsed() > shot_every {
            last_shot = Instant::now();
            let to = shots.join(format!("{n:04}.png"));
            if std::fs::copy(&screenshot, &to).is_ok() {
                if let Some((black, distinct)) = histogram(&to) {
                    eprintln!("[r] +{}s shot {} black {black:.3} colours {distinct}", started.elapsed().as_secs(), to.display());
                }
                n += 1;
            }
        }
        false
    };
    boot.watch(deadline.saturating_duration_since(Instant::now()), &mut on_line);
    // The device to be saved is quiet: shut it down, save it, and boot it again as a saved device
    // boots.
    if quiet.get() {
        if let Some(g) = &golden {
            std::thread::sleep(Duration::from_secs(2));
            boot.kill();
            save_device(&kept, g, &apk.display().to_string());
            for f in ["signed-in", "joining", "cookie-store", "cookie-planted"] {
                let _ = std::fs::remove_file(in_tmp(f));
            }
            boot = boot.reboot(&sysroot, &boot_args, &warm_then);
            boot.watch(deadline.saturating_duration_since(Instant::now()), &mut on_line);
        }
    }
    let tail = boot.tail();
    if stopped.get() {
        eprintln!("[r] stopped as asked (/data/local/tmp/stop)");
        return;
    }
    let missing: Vec<&String> = expect.iter().zip(&seen).filter(|(_, s)| !**s).map(|(e, _)| e).collect();
    assert!(missing.is_empty(), "never seen: {missing:?}\n{tail}");
    // A place asked for must have been joined: a run that only reached the app's Home, or its
    // "Upgrade required" screen (an APK the servers no longer accept), is not a pass.
    if place.is_some() {
        assert!(joined, "OMNI_R_PLACE was set but the log never showed \"Joining game\": the place was not joined with {} (an APK too old for the servers shows \"Upgrade required\")\n{tail}", apk.display());
    }
}

/// The runner's arguments for a boot: the zygote answered, the device's language (`persist.sys.locale=`),
/// and each of `OMNI_R_SETPROPS` (`name=value`, comma-separated: a lever tried at boot).
fn boot_args(locale: &str) -> Vec<String> {
    let mut args = vec!["--zygote".to_string(), "--setprop".to_string(), locale.to_string()];
    for kv in std::env::var("OMNI_R_SETPROPS").unwrap_or_default().split(',').filter(|kv| kv.contains('=')) {
        args.extend(["--setprop".to_string(), kv.trim().to_string()]);
    }
    args
}

/// `OMNI_R_WARM=1`: a **warm device** -- Android booted and idle, no app of its own, kept for the
/// apps a host program (the MCP server) installs, starts and stops through the device's control
/// channel (`<instance>.ctl`, `omni-linux-run --control`) one after another. The device is the
/// kiosk device the app sessions boot, less their APK: set up once (`settings_script`), saved
/// under `OMNI_R_GOLDEN` as `base-<kiosk|ui>-<locale>-v<DEVICE_SETUP>` and booted from a copy
/// afterwards. The host writes `/data/local/tmp/warm-ready` once it can take an app, and the
/// session runs `OMNI_R_MINUTES` (default 720) or until `/data/local/tmp/stop`.
fn warm_device(sysroot: &Path) {
    let minutes: u64 = std::env::var("OMNI_R_MINUTES").ok().and_then(|m| m.parse().ok()).unwrap_or(720);
    let instance = std::env::var_os("OMNI_R_INSTANCE")
        .map_or_else(|| std::env::temp_dir().join(format!("omni-linux-w-{}", std::process::id())), PathBuf::from);
    let _ = std::fs::remove_dir_all(&instance);
    let screenshot = std::env::var_os("OMNI_SCREENSHOT").map(PathBuf::from).unwrap_or_else(|| instance.with_extension("png"));
    std::env::set_var("OMNI_SCREENSHOT", &screenshot);
    let kiosk = std::env::var("OMNI_R_KIOSK").as_deref() != Ok("0");
    if kiosk {
        std::env::set_var("OMNI_DEVICE_APPS", "kiosk");
    }
    // A spare app process kept ready (`omni_linux::zygote`'s spare): the next app starts in it --
    // the probe's first frame 4.7 s sooner (6.5 against 10.9 s, 4 of 4 ABBA pairs), Roblox's
    // 2.9 s (2 of 2), 2026-10-01 -- for ~200-350 MB held while it waits. `OMNI_APP_SPARE=0`: none.
    if std::env::var_os("OMNI_APP_SPARE").is_none() {
        std::env::set_var("OMNI_APP_SPARE", "1");
    }
    // Per-app switches, read at each app's start (`omni_linux::zygote`): `<instance>.appenv`.
    if std::env::var_os("OMNI_APP_ENV_FILE").is_none() {
        std::env::set_var("OMNI_APP_ENV_FILE", instance.with_extension("appenv"));
    }
    let locale_name = std::env::var("OMNI_R_LOCALE").unwrap_or_else(|_| "tr-TR".into());
    let args = boot_args(&format!("persist.sys.locale={locale_name}"));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let root_req = root_request();
    let rooted_key = root_req.as_ref().map_or_else(String::new, |r| format!("-root-{}", r.hash));
    let golden = std::env::var_os("OMNI_R_GOLDEN")
        .map(|root| PathBuf::from(root).join(format!("base-{}-{locale_name}-v{DEVICE_SETUP}{rooted_key}{}{}", if kiosk { "kiosk" } else { "ui" }, omni_linux::boot_image::key_suffix(), omni_linux::device::idle_apps_key_suffix())));
    let saved = golden.as_ref().filter(|g| g.join("ready").exists());
    // Up: the screen woken, then ready for an app.
    let ready = format!("{BOOTED}input keyevent KEYCODE_WAKEUP; echo \"[r] warm ready\"; ");
    let then = if let Some(g) = saved {
        let t = Instant::now();
        let bytes = copy_device(&g.join("device"), &instance).expect("the saved device copied");
        eprintln!("[r] device from {}: {} MiB in {} ms", g.display(), bytes >> 20, t.elapsed().as_millis());
        ready.clone()
    } else {
        std::fs::create_dir_all(instance.join("data/local/tmp")).expect("/data/local/tmp");
        if let Some(request) = &root_req {
            stage_root(&instance, request);
        }
        let rest = if golden.is_some() { "sync; sleep 2; echo \"[r] device quiet\"; ".to_string() } else { "echo \"[r] warm ready\"; ".to_string() };
        format!("{BOOTED}{}{rest}", settings_script())
    };
    // A rooted warm device says which root profile it has (`omni_warm::ROOT_HASH_FILE`): a request
    // of another profile does not reuse it. An unrooted one writes nothing.
    if let Some(request) = &root_req {
        std::fs::write(instance.join("root-hash"), format!("{}\n", request.hash)).expect("the root hash");
    }
    let kept = instance.clone();
    let in_tmp = |name: &str| kept.join("data/local/tmp").join(name);
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    let quiet = std::cell::Cell::new(false);
    let mut on_line = |line: &str| -> bool {
        if line.contains("[r] warm ready") {
            let _ = std::fs::write(in_tmp("warm-ready"), "1");
        }
        if line.contains("[r] device quiet") {
            quiet.set(true);
            return true;
        }
        in_tmp("stop").exists()
    };
    let mut boot = common::boot::Boot::start(sysroot, instance, &args, &then);
    boot.watch(deadline.saturating_duration_since(Instant::now()), &mut on_line);
    if quiet.get() {
        if let Some(g) = &golden {
            std::thread::sleep(Duration::from_secs(2));
            boot.kill();
            save_device(&kept, g, "a warm device");
            boot = boot.reboot(sysroot, &args, &ready);
            boot.watch(deadline.saturating_duration_since(Instant::now()), &mut on_line);
        }
    }
    eprintln!("[r] warm device ended{}", if in_tmp("stop").exists() { " as asked (/data/local/tmp/stop)" } else { "" });
}

#[test]
fn golden_key_separates_rooted_devices_and_leaves_unrooted_unchanged() {
    use omni_linux::root::key::root_hash;
    let root = std::env::temp_dir();
    let apk = root.join("x.apk");
    let plain = golden_dir(&root, &apk, None, true, "tr-TR");
    // The unrooted key as it was before rooted devices existed.
    assert_eq!(plain, root.join(format!("x-0-guest-kiosk-tr-TR-v{DEVICE_SETUP}")), "the unrooted key is unchanged");
    let a = root_hash("root=1\nmodule=a\n", &[("a", "sha_a")], 29000, "bin");
    let b = root_hash("root=1\nmodule=a\nmodule=b\n", &[("a", "sha_a"), ("b", "sha_b")], 29000, "bin");
    let rooted = golden_dir_rooted(&root, &apk, None, true, "tr-TR", Some(&a));
    let rooted_b = golden_dir_rooted(&root, &apk, None, true, "tr-TR", Some(&b));
    assert_ne!(plain, rooted, "rooted differs from unrooted");
    assert_ne!(rooted, rooted_b, "the module set changes the key");
    assert!(rooted.to_string_lossy().ends_with(&format!("-root-{a}")));
    assert_eq!(plain, golden_dir_rooted(&root, &apk, None, true, "tr-TR", None));
    // The denylist is part of the staged profile text, so it keys another saved/warm device.
    let profile = |deny: &[&str]| {
        let mut p = omni_linux::root::Profile::parse("root=1
module=emu-hide
");
        for d in deny {
            p.denylist_add(d);
        }
        p.serialize()
    };
    let d0 = root_hash(&profile(&[]), &[], 29000, "bin");
    let d1 = root_hash(&profile(&["com.roblox.client"]), &[], 29000, "bin");
    assert_ne!(d0, d1, "a denylist change is a different rooted hash");
    assert_ne!(
        golden_dir_rooted(&root, &apk, None, true, "tr-TR", Some(&d0)),
        golden_dir_rooted(&root, &apk, None, true, "tr-TR", Some(&d1)),
        "and a different golden key"
    );
    assert_eq!(plain, golden_dir(&root, &apk, None, true, "tr-TR"), "the unrooted key stays byte-identical");
}
