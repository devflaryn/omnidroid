//! R: the owner's APK (`OMNI_TEST_APK`, else the stock `Roblox-2.738.1397.apk` in the repository
//! root) installed with `pm install` and started from its launcher on the real AOSP stack -- as a
//! device installs and starts it -- until its own UI is on the host display.
//!
//! The session is kept for the run's reader: every line in `<temp>/omni-linux-r-<pid>.log`, and the
//! display every 20 s in `<temp>/omni-linux-r-<pid>-shots/NNNN.png` (`tools/shot_stats.py` gives
//! their colour histograms). `OMNI_R_MINUTES` bounds the session (default 20); `OMNI_R_THEN`
//! is shell run after the launch (a deep link, a cookie); `OMNI_R_EXPECT` names the lines that must
//! have appeared, `|`-separated (default: the app's process started and a frame presented).
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

#[test]
#[ignore = "boots the whole system, installs and starts an app: many minutes"]
fn the_apk_is_installed_started_and_draws() {
    let Some(sysroot) = common::sysroot() else { return };
    let apk = std::env::var_os("OMNI_TEST_APK")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Roblox-2.738.1397.apk"), PathBuf::from);
    assert!(apk.exists(), "no APK at {}", apk.display());
    let minutes: u64 = std::env::var("OMNI_R_MINUTES").ok().and_then(|m| m.parse().ok()).unwrap_or(20);
    let tag = format!("omni-linux-r-{}", std::process::id());
    let instance = std::env::temp_dir().join(&tag);
    let _ = std::fs::remove_dir_all(&instance);
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).expect("/data/local/tmp");
    std::fs::copy(&apk, tmp.join("app.apk")).expect("the APK");
    let screenshot = instance.with_extension("png");
    let shots = std::env::temp_dir().join(format!("{tag}-shots"));
    let _ = std::fs::create_dir_all(&shots);
    std::env::set_var("OMNI_SCREENSHOT", &screenshot);

    let extra = std::env::var("OMNI_R_THEN").unwrap_or_default();
    // `OMNI_R_COOKIE=<file>`: the app's first start makes its own cookie store; the app is stopped,
    // the session cookie put in that store (`tools/plant_cookie.py`, never printed), and the app
    // started again -- as a device that had signed in starts it.
    let cookie = std::env::var_os("OMNI_R_COOKIE").map(PathBuf::from);
    let sign_in = if cookie.is_some() {
        "sleep 45; am force-stop com.roblox.client; echo \"[r] cookie-stop\"; \
         i=0; until [ -e /data/local/tmp/cookie-planted ] || [ $i -ge 180 ]; do sleep 1; i=$((i+1)); done; \
         echo \"[r] cookie planted: $(cat /data/local/tmp/cookie-planted)\"; \
         am start -W -n \"$act\"; echo \"[r] relaunched: $?\"; "
    } else {
        ""
    };
    // `OMNI_R_PLACE=<id>`: once signed in, the place's deep link, as a link opened on a device.
    let join = std::env::var("OMNI_R_PLACE").ok().map_or_else(String::new, |id| {
        format!(
            "i=0; until [ -e /data/local/tmp/signed-in ] || [ $i -ge 900 ]; do sleep 1; i=$((i+1)); done; sleep 45; \
             am start -a android.intent.action.VIEW -d 'roblox://experiences/start?placeId={id}' -n com.roblox.client/com.roblox.client.ActivityProtocolLaunch; \
             echo \"[r] join intent for {id}: $?\"; "
        )
    });
    // A test device trimmed as test images are (`OMNI_R_LEAN=0` keeps everything): the image's apps
    // that nothing here uses are disabled, so they neither run nor come back -- each app host
    // process holds ~263 MiB, and with ~20 of them started at boot the host ran out of memory as
    // the game loaded (r11: two sessions stopped by Claude Code's low-memory reaper).
    const IDLE_APPS: &[&str] = &[
        "com.android.calendar", "com.android.providers.calendar", "com.android.contacts", "com.android.camera2",
        "com.android.messaging", "com.android.printspooler", "com.android.cellbroadcastreceiver",
        "com.android.cellbroadcastreceiver.module", "com.android.dynsystem", "com.android.traceur", "com.android.nfc",
        "com.android.imsserviceentitlement", "com.android.healthconnect.controller", "com.android.ondevicepersonalization.services",
        "com.android.devicelockcontroller", "com.android.statementservice", "com.android.documentsui",
        "com.android.localtransport", "com.android.emulator.multidisplay", "com.android.federatedcompute.services",
        "com.android.adservices.api", "com.android.bluetooth", "com.android.managedprovisioning",
        "com.android.se", "com.android.rkpdapp", "com.android.externalstorage", "com.android.keychain",
        "com.android.ext.adservices.api", "com.android.providers.contacts", "com.android.providers.blockednumber",
        "com.android.providers.userdictionary", "com.android.wallpaperbackup", "com.android.carrierconfig",
        "com.android.providers.telephony", "com.android.stk", "com.android.mms.service", "com.android.cellbroadcastservice",
    ];
    let lean = if std::env::var("OMNI_R_LEAN").as_deref() == Ok("0") {
        String::new()
    } else {
        format!(
            "for a in {}; do r=$(cmd package disable-user --user 0 $a 2>&1); case \"$r\" in *disabled*) ;; *) echo \"[r] not disabled: $a: $r\";; esac; done; echo \"[r] idle apps disabled: {}\"; ",
            IDLE_APPS.join(" "),
            IDLE_APPS.len()
        )
    };
    // After the install: the package installer, which the install itself needed.
    let lean_after = if lean.is_empty() { String::new() } else { "cmd package disable-user --user 0 com.android.packageinstaller >/dev/null 2>&1; ".to_string() };
    let then = format!(
        "i=0; until [ \"$(getprop sys.boot_completed)\" = 1 ] || [ $i -ge 240 ]; do sleep 5; i=$((i+1)); done; \
         echo \"[r] boot_completed=$(getprop sys.boot_completed)\"; \
         settings put global device_provisioned 1; settings put secure user_setup_complete 1; \
         settings put system screen_off_timeout 1800000; svc power stayon true; \
         input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard; \
         settings put global window_animation_scale 0; settings put global transition_animation_scale 0; \
         settings put global animator_duration_scale 0; settings put secure immersive_mode_confirmations confirmed; \
         {lean}pm install -r -g /data/local/tmp/app.apk; echo \"[r] pm install: $?\"; \
         {lean_after}
         pkg=$(pm list packages -3 | head -1 | sed 's/^package://'); echo \"[r] package $pkg\"; \
         act=$(cmd package resolve-activity --brief -c android.intent.category.LAUNCHER \"$pkg\" | tail -1); echo \"[r] launcher $act\"; \
         am start -W -n \"$act\"; echo \"[r] am start: $?\"; \
         {sign_in}{join}{extra}"
    );
    let kept = instance.clone();
    // The device's language: the account's (`OMNI_R_LOCALE`, default tr-TR -- the owner's accounts
    // are Turkish). The app applies the account's locale to itself once signed in; on a device in
    // another language that is a configuration change ActivityNativeMain does not handle
    // (configChanges 0xfb0 leaves locale out), and the relaunch ended the game session the deep link
    // had just started (r12: "Updating App configuration based on locale tr_tr", then "Schedule
    // relaunch activity", "Ending game session with place ID 8737899170").
    let locale = format!("persist.sys.locale={}", std::env::var("OMNI_R_LOCALE").unwrap_or_else(|_| "tr-TR".into()));
    let mut boot = common::boot::Boot::start(&sysroot, instance, &["--zygote", "--setprop", &locale], &then);
    let expect: Vec<String> = std::env::var("OMNI_R_EXPECT")
        .unwrap_or_else(|_| "[zygote] launching com.roblox.client|frames presented".into())
        .split('|')
        .map(String::from)
        .collect();
    let mut seen = vec![false; expect.len()];
    let started = Instant::now();
    let mut last_shot = Instant::now();
    let mut n = 0;
    boot.watch(Duration::from_secs(minutes * 60), |line| {
        for (s, e) in seen.iter_mut().zip(&expect) {
            *s |= line.contains(e.as_str());
        }
        if line.contains("[r] cookie-stop") {
            if let Some(file) = &cookie {
                std::thread::sleep(Duration::from_secs(3));
                let store = kept.join("data/data/com.roblox.client/app_webview/Default/Cookies");
                let tool = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/plant_cookie.py");
                let planted = std::process::Command::new("python").arg(&tool).arg(&store).arg(file).output();
                let said = match &planted {
                    Ok(o) => format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
                    Err(e) => format!("python: {e}"),
                };
                eprintln!("[r] {}", said.trim());
                let ok = planted.is_ok_and(|o| o.status.success());
                let _ = std::fs::write(kept.join("data/local/tmp/cookie-planted"), if ok { "ok" } else { "failed" });
            }
        }
        if line.contains("DID_LOG_IN") && !kept.join("data/local/tmp/signed-in").exists() {
            let _ = std::fs::write(kept.join("data/local/tmp/signed-in"), "1");
        }
        if last_shot.elapsed() > Duration::from_secs(20) {
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
    });
    let tail = boot.tail();
    let missing: Vec<&String> = expect.iter().zip(&seen).filter(|(_, s)| !**s).map(|(e, _)| e).collect();
    assert!(missing.is_empty(), "never seen: {missing:?}\n{tail}");
}
