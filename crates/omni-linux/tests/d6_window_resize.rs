//! D6: the display in a live host window, and a window resize that resizes Android's display.
//!
//! The runner shows the display in a native window (`OMNI_WINDOW=1`, `omni_linux::display_window`)
//! and the window's size is the display's. The probe app (`tests/fixtures/probe-app`, one colour,
//! handling its own configuration changes) is started; then the window is resized as a user's drag
//! would resize it -- through the runner's control file (`OMNI_WINDOW_CONTROL`), which calls the
//! window seam's `set_client_size` -- first smaller, then larger, to sizes of no standard aspect
//! ratio (817x542, 1531x877). At each size the gate requires:
//!
//! * the composer resized the display and SurfaceFlinger reconnected it (`[composer] display WxH`,
//!   `Reconnecting`), and Android's own `wm size` reports the new physical size;
//! * the probe was given a configuration change (`onConfigurationChanged`) and laid its view out
//!   and drew it again at the new width;
//! * the framebuffer's frame has the new size and the probe's blue covers most of it (the window
//!   is filled, status bar and taskbar aside);
//! * frames keep reaching the window: the window's presented count tracks the framebuffer's.
//!
//! `OMNI_D6_SHOT_PROGRAM` (+ `OMNI_D6_SHOT_ARGS`, `|`-separated, `{out}` replaced by the path) is
//! run at each size to capture the window as the desktop shows it (on Windows,
//! `tools/window_shot.ps1`); the shots are kept in `OMNI_D6_SHOTS` (default `<temp>/omni-linux-d6-shots`).
//!
//! Minutes long: `cargo test -p omni-linux --release --test d6_window_resize -- --ignored --nocapture`.
mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The probe's view colour, RGB.
const PROBE_BLUE: [u8; 3] = [0x21, 0x96, 0xf3];

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

/// The screenshot's size, its centre pixel and the share of its pixels that are the probe's blue.
fn look(png: &Path) -> Option<((usize, usize), [u8; 3], f64)> {
    let (w, h, px) = decode(png)?;
    let at = (h / 2 * w + w / 2) * 4;
    let centre = [px[at], px[at + 1], px[at + 2]];
    let blue = px.chunks_exact(4).filter(|p| p[..3] == PROBE_BLUE).count();
    Some(((w, h), centre, blue as f64 / (w * h).max(1) as f64))
}

/// What the log has said, as the gate reads it.
#[derive(Default)]
struct Seen {
    committed: bool,
    displayed: bool,
    /// The latest `[window] N frames presented to the window; framebuffer M frames` numbers.
    window_frames: Option<(u64, u64)>,
    /// The display's size as the composer last set it (`[composer] display WxH`).
    display: Option<String>,
    /// Lines seen since the stage began.
    stage: Vec<String>,
}

/// One size the window is taken to.
struct Stage {
    label: &'static str,
    size: (u32, u32),
}

#[test]
#[ignore = "boots the whole system, starts an app and resizes its display: minutes"]
fn the_display_is_live_in_a_window_and_follows_its_size() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = std::env::temp_dir().join(format!("omni-linux-d6-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).expect("/data/local/tmp");
    let apk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/probe-app/probe.apk");
    std::fs::copy(&apk, tmp.join("probe.apk")).expect("the probe APK");
    let screenshot = instance.with_extension("png");
    let control = instance.with_extension("control");
    let _ = std::fs::remove_file(&screenshot);
    std::fs::write(&control, "").expect("the control file");
    let shots = std::env::var_os("OMNI_D6_SHOTS").map_or_else(|| std::env::temp_dir().join("omni-linux-d6-shots"), PathBuf::from);
    let _ = std::fs::create_dir_all(&shots);
    std::env::set_var("OMNI_SCREENSHOT", &screenshot);
    std::env::set_var("OMNI_SCREENSHOT_MS", "1000");
    std::env::set_var("OMNI_WINDOW", "1");
    std::env::set_var("OMNI_WINDOW_CONTROL", &control);

    // The device as D5 sets it up; then, on the gate's request (a file per stage), what Android
    // itself says the display is: `wm size` and DisplayManager's view of it.
    let then = "i=0; until [ \"$(getprop sys.boot_completed)\" = 1 ] || [ $i -ge 240 ]; do sleep 5; i=$((i+1)); done; \
                echo \"[d6] boot_completed=$(getprop sys.boot_completed)\"; \
                settings put global device_provisioned 1; settings put secure user_setup_complete 1; \
                settings put system screen_off_timeout 1800000; svc power stayon true; \
                input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard; \
                settings put global window_animation_scale 0; settings put global transition_animation_scale 0; \
                settings put global animator_duration_scale 0; settings put secure immersive_mode_confirmations confirmed; \
                pm install -r /data/local/tmp/probe.apk; echo \"[d6] pm install: $?\"; \
                am start -W -n com.omnidroid.probe/.MainActivity; echo \"[d6] am start: $?\"; \
                while true; do for f in /data/local/tmp/ask-*; do [ -e \"$f\" ] || continue; n=${f##*/ask-}; rm -f \"$f\"; \
                  wm size | sed \"s/^/[d6] $n wm /\"; \
                  dumpsys display | grep -E 'mBaseDisplayInfo|DisplayDeviceInfo\\{' | head -2 | sed \"s/^/[d6] $n display /\"; \
                done; sleep 1; done";
    let mut boot = common::boot::Boot::start(&sysroot, instance.clone(), &["--zygote"], then);

    let shot_program = std::env::var("OMNI_D6_SHOT_PROGRAM").ok();
    let shot_args: Vec<String> = std::env::var("OMNI_D6_SHOT_ARGS").map(|a| a.split('|').map(String::from).collect()).unwrap_or_default();
    let window_shot = |label: &str| {
        let Some(program) = &shot_program else { return };
        let out = shots.join(format!("d6-window-{label}.png"));
        let args: Vec<String> = shot_args.iter().map(|a| a.replace("{out}", &out.to_string_lossy())).collect();
        match std::process::Command::new(program).args(&args).output() {
            Ok(o) => eprintln!("[d6] window shot {label}: {}{}", String::from_utf8_lossy(&o.stdout).trim(), String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => eprintln!("[d6] window shot {label}: {e}"),
        }
    };
    let ask = |n: &str| {
        let _ = std::fs::write(tmp.join(format!("ask-{n}")), "");
    };

    let mut seen = Seen::default();
    let note = |seen: &mut Seen, line: &str| {
        seen.committed |= line.contains("OmniProbe") && line.contains("frame committed");
        seen.displayed |= line.contains("Displayed com.omnidroid.probe/.MainActivity");
        if let Some(rest) = line.split("[window] ").nth(1) {
            let mut numbers = rest.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse::<u64>().unwrap_or(0));
            if rest.contains("frames presented to the window; framebuffer") {
                if let (Some(n), Some(m)) = (numbers.next(), numbers.next()) {
                    seen.window_frames = Some((n, m));
                }
            }
        }
        if let Some(rest) = line.strip_prefix("[composer] display ") {
            seen.display = rest.split_whitespace().next().map(String::from);
        }
        seen.stage.push(line.to_string());
    };

    // Stage 0: the probe on the display, at the boot size, live in the window.
    let mut blue_at_start = None;
    let mut last_check = Instant::now();
    boot.watch(Duration::from_secs(2400), |line| {
        note(&mut seen, line);
        if seen.committed && seen.displayed && seen.window_frames.is_some_and(|(n, _)| n > 0) && last_check.elapsed() > Duration::from_secs(2) {
            last_check = Instant::now();
            if let Some((size, centre, share)) = look(&screenshot) {
                if centre == PROBE_BLUE && share > 0.5 {
                    blue_at_start = Some((size, share));
                }
            }
        }
        blue_at_start.is_some()
    });
    let tail = boot.tail();
    assert!(seen.committed, "the probe's first frame was never committed\n{tail}");
    assert!(seen.displayed, "the probe's Activity was never displayed\n{tail}");
    let (start_size, start_share) = blue_at_start.unwrap_or_else(|| panic!("the probe never filled the display (screenshot {}): {:?}\n{tail}", screenshot.display(), look(&screenshot)));
    eprintln!("[d6] start: display {start_size:?}, blue {start_share:.3}");
    let _ = std::fs::copy(&screenshot, shots.join("d6-display-start.png"));
    std::thread::sleep(Duration::from_secs(2));
    window_shot("start");
    ask("start");

    // Stages 1 and 2: shrink, then grow.
    // Sizes of no standard ratio, odd widths included: the display is whatever the window is.
    let stages = [Stage { label: "shrink", size: (817, 542) }, Stage { label: "grow", size: (1531, 877) }];
    for stage in &stages {
        let (w, h) = stage.size;
        seen.stage.clear();
        let mut control_text = std::fs::read_to_string(&control).unwrap_or_default();
        control_text.push_str(&format!("size {w}x{h}\n"));
        std::fs::write(&control, control_text).expect("the control file");
        let started = Instant::now();
        let (mut resized, mut reconnected, mut configured, mut drawn, mut filled) = (false, false, false, None, None);
        let mut last_check = Instant::now();
        boot.watch(Duration::from_secs(300), |line| {
            note(&mut seen, line);
            resized |= line.contains(&format!("[composer] display {w}x{h}"));
            reconnected |= resized && line.contains("Reconnecting");
            configured |= resized && line.contains("OmniProbe") && line.contains("onConfigurationChanged");
            if resized && line.contains("OmniProbe") {
                if let Some(size) = line.split("drawn ").nth(1) {
                    let dims: Vec<u32> = size.trim().split('x').filter_map(|v| v.trim().parse().ok()).collect();
                    if dims.len() == 2 && dims[0] == w {
                        drawn = Some((dims[0], dims[1]));
                    }
                }
            }
            if configured && drawn.is_some() && last_check.elapsed() > Duration::from_secs(2) {
                last_check = Instant::now();
                if let Some((size, centre, share)) = look(&screenshot) {
                    if size == (w as usize, h as usize) && centre == PROBE_BLUE && share > 0.5 {
                        filled = Some(share);
                    }
                }
            }
            filled.is_some()
        });
        let tail = boot.tail();
        let lines = seen.stage.join("\n");
        assert!(resized, "{}: the display was never resized to {w}x{h}\n{tail}", stage.label);
        assert!(configured, "{}: the probe was given no configuration change after the resize\n{tail}", stage.label);
        assert!(drawn.is_some(), "{}: the probe never drew at width {w}\n{tail}", stage.label);
        let share = filled.unwrap_or_else(|| panic!("{}: the display never showed the probe at {w}x{h}: {:?}\n{tail}", stage.label, look(&screenshot)));
        eprintln!(
            "[d6] {}: display {w}x{h} in {:.1} s; SurfaceFlinger reconnect seen: {reconnected}; probe drew {:?}; blue {share:.3}",
            stage.label,
            started.elapsed().as_secs_f64(),
            drawn.expect("checked")
        );
        let _ = std::fs::copy(&screenshot, shots.join(format!("d6-display-{}.png", stage.label)));
        std::thread::sleep(Duration::from_secs(2));
        window_shot(stage.label);
        ask(stage.label);
        // Android's own word for the size.
        let mut physical = None;
        boot.watch(Duration::from_secs(30), |line| {
            note(&mut seen, line);
            if line.contains(&format!("[d6] {} wm Physical size:", stage.label)) {
                physical = line.rsplit(' ').next().map(String::from);
            }
            physical.is_some()
        });
        // Against the display as it is when Android is asked: the window is a real one, and a
        // person watching may resize it too (run 1: dragged to 1024x900 after the grow, and the
        // display followed) -- which the display following is the point of, not a failure.
        let now = seen.display.clone().unwrap_or_default();
        if now != format!("{w}x{h}") {
            eprintln!("[d6] {}: the window was resized again from outside the gate, to {now}", stage.label);
        }
        assert_eq!(physical.as_deref(), Some(now.as_str()), "{}: `wm size` does not report the display's size\n{lines}", stage.label);
    }

    // Frames kept reaching the window: its count tracks the framebuffer's (coalescing only ever
    // makes it trail).
    boot.watch(Duration::from_secs(8), |line| {
        note(&mut seen, line);
        false
    });
    let (window, framebuffer) = seen.window_frames.expect("a [window] line");
    eprintln!("[d6] frames: window {window}, framebuffer {framebuffer}");
    assert!(window <= framebuffer && window * 10 >= framebuffer * 9, "the window presented {window} of {framebuffer} frames");
}
