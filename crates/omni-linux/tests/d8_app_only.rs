//! D8: **only the app** on the host display -- no status bar, no navigation bar, no taskbar.
//!
//! The probe app (`tests/fixtures/probe-app`: an action bar over one blue view) is started on the
//! live window (`OMNI_WINDOW=1`) under SystemUI and the launcher's taskbar, as D6 starts it. Then:
//!
//! 1. **App only** (the composer's default): the composer leaves the system's chrome layers out
//!    (`hal::composer`, "Only the app"). The top strip, where SystemUI's status bar is, holds no
//!    status bar (none of its white clock and icons: the probe's own dark bar colour instead), and
//!    the bottom strip, where the taskbar is, none of the taskbar's near-white bar.
//! 2. **`chrome show`** (the control file): the same device, the whole display presented -- the
//!    status bar's white clock and icons in the top strip and the taskbar's near-white bar in the
//!    bottom one: the regions of stage 1 did hold the chrome, and it was left out.
//! 3. **`chrome hide`**: app only again, live.
//! 4. **Kiosk** (`OMNI_D8_KIOSK=0` skips it): SystemUI disabled as a dedicated single-app device is
//!    set up (`pm disable-user com.android.systemui`), and the device started again (while it
//!    runs, SystemUI's death locks the device: Android shows the keyguard when the keyguard's
//!    service dies). No SystemUI, no bars, no taskbar (the launcher's taskbar lives only while
//!    SystemUI binds its service), and the whole display presented (`OMNI_APP_ONLY=0`):
//!    SurfaceFlinger has no `StatusBar`, `NavigationBar` or `Taskbar` layer, and the probe is given
//!    the whole display: its action bar at the top edge, its blue down to the bottom edge (more of
//!    the frame blue than in stage 1).
//!
//! The frames checked are the framebuffer's, which are the window's (D6: window frames =
//! framebuffer frames). `OMNI_D8_SHOT_PROGRAM` (+ `OMNI_D8_SHOT_ARGS`, `|`-separated, `{out}` the
//! path) captures the window as the desktop shows it at each stage (on Windows,
//! `tools/window_shot.ps1`; `tools/chrome_check.py` measures the same regions on those captures);
//! all shots are kept in `OMNI_D8_SHOTS` (default `<temp>/omni-linux-d8-shots`).
//!
//! Minutes long: `cargo test -p omni-linux --release --test d8_app_only -- --ignored --nocapture`.
mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The probe's view colour, RGB.
const PROBE_BLUE: [u8; 3] = [0x21, 0x96, 0xf3];
/// The status bar's height at this display's 160 dpi (24 dp), and the visible taskbar's (56 dp):
/// the two strips the chrome occupies.
const TOP_STRIP: usize = 24;
const BOTTOM_STRIP: usize = 56;

/// Size and pixels (RGBA) of the runner's screenshot, if one is there and whole.
fn decode(png: &Path) -> Option<(usize, usize, Vec<u8>)> {
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
    let mut px = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        px.extend_from_slice(raw.get(y * (width * 4 + 1) + 1..(y + 1) * (width * 4 + 1))?);
    }
    Some((width, height, px))
}

/// What a region of a frame holds: its share of near-white pixels (every channel >= 235: the
/// status bar's clock and icons, the taskbar's bar) and of the probe's blue.
#[derive(Debug, Clone, Copy)]
struct Region {
    white: f64,
    blue: f64,
}

/// The frame's top and bottom strips and its centre pixel.
#[derive(Debug, Clone, Copy)]
struct Look {
    #[allow(dead_code)] // in the printed look
    size: (usize, usize),
    centre: [u8; 3],
    top: Region,
    bottom: Region,
    /// The share of the whole frame that is the probe's blue.
    blue: f64,
}

fn region(w: usize, px: &[u8], rows: std::ops::Range<usize>) -> Region {
    let (mut white, mut blue, mut n) = (0usize, 0usize, 0usize);
    for y in rows {
        for p in px[y * w * 4..(y + 1) * w * 4].chunks_exact(4) {
            n += 1;
            white += usize::from(p[..3].iter().all(|&c| c >= 235));
            blue += usize::from(p[..3] == PROBE_BLUE);
        }
    }
    Region { white: white as f64 / n.max(1) as f64, blue: blue as f64 / n.max(1) as f64 }
}

fn look(png: &Path) -> Option<Look> {
    let (w, h, px) = decode(png)?;
    if h < TOP_STRIP + BOTTOM_STRIP {
        return None;
    }
    let at = (h / 2 * w + w / 2) * 4;
    Some(Look {
        size: (w, h),
        centre: [px[at], px[at + 1], px[at + 2]],
        top: region(w, &px, 0..TOP_STRIP),
        bottom: region(w, &px, h - BOTTOM_STRIP..h),
        blue: region(w, &px, 0..h).blue,
    })
}

/// The chrome is absent from a frame: nothing of the status bar's white in the top strip and
/// nearly nothing of the taskbar's in the bottom one.
fn chrome_absent(l: &Look) -> bool {
    l.centre == PROBE_BLUE && l.top.white < 0.001 && l.bottom.white < 0.05
}

/// The chrome is on a frame: the status bar's clock and icons, the taskbar's bar.
fn chrome_present(l: &Look) -> bool {
    l.centre == PROBE_BLUE && l.top.white > 0.002 && l.bottom.white > 0.5
}

/// What the log has said.
#[derive(Default)]
struct Seen {
    committed: bool,
    displayed: bool,
}

impl Seen {
    fn note(&mut self, line: &str) {
        self.committed |= line.contains("OmniProbe") && line.contains("frame committed");
        self.displayed |= line.contains("Displayed com.omnidroid.probe/.MainActivity");
    }
}

#[test]
#[ignore = "boots the whole system, starts an app and looks at what the window presents: minutes"]
fn only_the_app_is_presented_edge_to_edge() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = std::env::temp_dir().join(format!("omni-linux-d8-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).expect("/data/local/tmp");
    let apk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/probe-app/probe.apk");
    std::fs::copy(&apk, tmp.join("probe.apk")).expect("the probe APK");
    let screenshot = instance.with_extension("png");
    let control = instance.with_extension("control");
    let _ = std::fs::remove_file(&screenshot);
    std::fs::write(&control, "").expect("the control file");
    let shots = std::env::var_os("OMNI_D8_SHOTS").map_or_else(|| std::env::temp_dir().join("omni-linux-d8-shots"), PathBuf::from);
    let _ = std::fs::create_dir_all(&shots);
    std::env::set_var("OMNI_SCREENSHOT", &screenshot);
    std::env::set_var("OMNI_SCREENSHOT_MS", "1000");
    std::env::set_var("OMNI_WINDOW", "1");
    std::env::set_var("OMNI_WINDOW_CONTROL", &control);
    std::env::remove_var("OMNI_APP_ONLY");

    // The device as D5 sets it up; then, on the gate's request (a file), SystemUI disabled and
    // the package state given time to be written (PackageManager writes it after a delay).
    let then = "i=0; until [ \"$(getprop sys.boot_completed)\" = 1 ] || [ $i -ge 240 ]; do sleep 5; i=$((i+1)); done; \
                echo \"[d8] boot_completed=$(getprop sys.boot_completed)\"; \
                settings put global device_provisioned 1; settings put secure user_setup_complete 1; \
                settings put system screen_off_timeout 1800000; svc power stayon true; \
                input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard; \
                settings put global window_animation_scale 0; settings put global transition_animation_scale 0; \
                settings put global animator_duration_scale 0; settings put secure immersive_mode_confirmations confirmed; \
                pm install -r /data/local/tmp/probe.apk; echo \"[d8] pm install: $?\"; \
                am start -W -n com.omnidroid.probe/.MainActivity; echo \"[d8] am start: $?\"; \
                while true; do \
                  if [ -e /data/local/tmp/ask-kiosk ]; then rm -f /data/local/tmp/ask-kiosk; \
                    echo \"[d8] kiosk: $(pm disable-user --user 0 com.android.systemui 2>&1)\"; \
                    sleep 15; sync; echo \"[d8] kiosk kept\"; fi; \
                  sleep 1; done";
    let mut boot = common::boot::Boot::start(&sysroot, instance.clone(), &["--zygote"], then);

    let shot_program = std::env::var("OMNI_D8_SHOT_PROGRAM").ok();
    let shot_args: Vec<String> = std::env::var("OMNI_D8_SHOT_ARGS").map(|a| a.split('|').map(String::from).collect()).unwrap_or_default();
    let keep = |label: &str| {
        let _ = std::fs::copy(&screenshot, shots.join(format!("d8-display-{label}.png")));
        let Some(program) = &shot_program else { return };
        std::thread::sleep(Duration::from_secs(1));
        let out = shots.join(format!("d8-window-{label}.png"));
        let args: Vec<String> = shot_args.iter().map(|a| a.replace("{out}", &out.to_string_lossy())).collect();
        match std::process::Command::new(program).args(&args).output() {
            Ok(o) => eprintln!("[d8] window shot {label}: {}{}", String::from_utf8_lossy(&o.stdout).trim(), String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => eprintln!("[d8] window shot {label}: {e}"),
        }
    };
    let command = |line: &str| {
        let mut text = std::fs::read_to_string(&control).unwrap_or_default();
        text.push_str(line);
        text.push('\n');
        std::fs::write(&control, text).expect("the control file");
    };

    let mut seen = Seen::default();

    // Wait, at most `limit`, for a frame `ok` accepts: that frame's look, or the last one seen.
    let wait_for = |boot: &mut common::boot::Boot, seen: &mut Seen, limit: Duration, ok: &dyn Fn(&Look) -> bool| -> (bool, Option<Look>) {
        let (mut last, mut found) = (None, false);
        let mut checked = Instant::now();
        boot.watch(limit, |line| {
            seen.note(line);
            if checked.elapsed() > Duration::from_secs(1) {
                checked = Instant::now();
                last = look(&screenshot);
                found = last.as_ref().is_some_and(ok);
            }
            found
        });
        (found, last)
    };

    // Stage 1: app only, the default.
    let (ok, l) = wait_for(&mut boot, &mut seen, Duration::from_secs(2400), &|l| chrome_absent(l));
    let tail = boot.tail();
    assert!(seen.committed && seen.displayed, "the probe never came up (committed {}, displayed {})\n{tail}", seen.committed, seen.displayed);
    assert!(ok, "the display never showed the probe without the chrome: {l:?}\n{tail}");
    // Held for a few frames: not a moment before the chrome was first drawn.
    std::thread::sleep(Duration::from_secs(3));
    let (ok, app_only) = wait_for(&mut boot, &mut seen, Duration::from_secs(20), &|l| chrome_absent(l));
    assert!(ok, "app only: the chrome came back: {app_only:?}\n{}", boot.tail());
    eprintln!("[d8] app only: {app_only:?}");
    keep("app-only");

    // Stage 2: the whole display, for the regions' contrast.
    command("chrome show");
    let (ok, whole) = wait_for(&mut boot, &mut seen, Duration::from_secs(60), &|l| chrome_present(l));
    assert!(ok, "with `chrome show` the status bar and taskbar never appeared in the strips: {whole:?}\n{}", boot.tail());
    eprintln!("[d8] chrome shown: {whole:?}");
    keep("chrome-shown");

    // Stage 3: app only again, live.
    command("chrome hide");
    let (ok, again) = wait_for(&mut boot, &mut seen, Duration::from_secs(60), &|l| chrome_absent(l));
    assert!(ok, "with `chrome hide` the chrome stayed: {again:?}\n{}", boot.tail());
    eprintln!("[d8] chrome hidden again: {again:?}");

    if std::env::var("OMNI_D8_KIOSK").as_deref() == Ok("0") {
        return;
    }
    // Stage 4: a device without SystemUI. SystemUI disabled, as a dedicated device is set up, and
    // the device started again (disabling it while it runs locks the device: Android shows the
    // keyguard when the keyguard's service dies). Then the whole display is presented: there is
    // no chrome left to leave out.
    let _ = std::fs::write(tmp.join("ask-kiosk"), "");
    let mut disabled = None;
    boot.watch(Duration::from_secs(90), |line| {
        seen.note(line);
        if let Some(said) = line.split("[d8] kiosk: ").nth(1) {
            disabled = Some(said.to_string());
        }
        line.contains("[d8] kiosk kept")
    });
    let disabled = disabled.unwrap_or_default();
    assert!(disabled.contains("disabled"), "SystemUI could not be disabled: {disabled:?}\n{}", boot.tail());
    std::env::set_var("OMNI_APP_ONLY", "0");
    let then = "i=0; until [ \"$(getprop sys.boot_completed)\" = 1 ] || [ $i -ge 240 ]; do sleep 5; i=$((i+1)); done; \
                echo \"[d8] kiosk boot_completed=$(getprop sys.boot_completed); SystemUI disabled: $(pm list packages -d | grep -c com.android.systemui)\"; \
                input keyevent KEYCODE_WAKEUP; \
                am start -W -n com.omnidroid.probe/.MainActivity; echo \"[d8] am start: $?\"; sleep 20; \
                dumpsys SurfaceFlinger --list | sed 's/^/[d8] layer /'; echo \"[d8] layers listed\"; \
                dumpsys window | grep -E 'mCurrentFocus|isKeyguardShowing|mShowingDream' | sed 's/^/[d8] wm /'";
    let mut boot = boot.reboot(&sysroot, &["--zygote"], then);
    seen = Seen::default();
    let (ok, kiosk) = wait_for(&mut boot, &mut seen, Duration::from_secs(1200), &|l| l.centre == PROBE_BLUE && l.bottom.blue > 0.9 && l.bottom.white < 0.05);
    let tail = boot.tail();
    assert!(seen.displayed, "without SystemUI the probe was never displayed\n{tail}");
    assert!(ok, "without SystemUI the probe never reached the display's bottom edge: {kiosk:?}\n{tail}");
    eprintln!("[d8] kiosk: {kiosk:?}");
    keep("kiosk");
    let mut layers = Vec::new();
    let mut listed = false;
    boot.watch(Duration::from_secs(120), |line| {
        seen.note(line);
        if let Some(layer) = line.split("[d8] layer ").nth(1) {
            layers.push(layer.to_string());
        }
        listed |= line.contains("[d8] layers listed");
        listed
    });
    assert!(listed, "SurfaceFlinger's layers were never listed\n{}", boot.tail());
    let chrome: Vec<&String> = layers.iter().filter(|l| l.contains("StatusBar") || l.contains("Taskbar") || l.contains("NavigationBar")).collect();
    assert!(chrome.is_empty(), "without SystemUI SurfaceFlinger still has chrome layers: {chrome:?}");
    // The app was given the display the bars had held: more of the frame is its view.
    let (before, after) = (app_only.map_or(0.0, |l| l.blue), kiosk.map_or(0.0, |l| l.blue));
    assert!(after > before + 0.05, "the probe was not given more of the display without SystemUI: blue {before:.3} -> {after:.3}");
}
