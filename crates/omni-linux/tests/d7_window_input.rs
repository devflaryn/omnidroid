//! D7: the live window's keyboard and mouse reach the app as a device's keyboard and mouse.
//!
//! The runner shows the display in its window with input on (`OMNI_WINDOW=1`,
//! `display_window`/`window_input`): `/dev/input/event0` a keyboard and `event1` a relative mouse,
//! read by system_server's own `InputReader`, and the absolute pointer, whose events the host
//! injects into the input dispatcher (`inject`). Through the runner's control file -- the same
//! translation the window's own events take -- the gate presses A, clicks at (400, 300), points at
//! (500, 280), turns the wheel a notch, clicks at (1000, 150); then presses C, on which the probe
//! app asks for the pointer capture, moves the mouse by (+30, -10) and presses R, on which it
//! releases it. The probe (`tests/fixtures/probe-app`) logs what reaches it, and the gate requires:
//!
//! * Android lists both evdev devices (`dumpsys input`: `omnidroid keyboard`, `omnidroid mouse`);
//! * the key as a keyboard's: `KEYCODE_A` (29), scan code `KEY_A` (30), source `SOURCE_KEYBOARD`;
//! * the clicks as a mouse's, **exactly where they were made**, with nothing captured: `ACTION_DOWN`
//!   from `SOURCE_MOUSE` (0x2002), tool `TOOL_TYPE_MOUSE`, primary button, at (400, 300) and at
//!   (1000, 150) within a pixel, and the `ACTION_UP`s;
//! * the pointer free and absolute: an `ACTION_HOVER_MOVE` at (500, 280);
//! * the wheel: an `ACTION_SCROLL` of one notch up;
//! * the app's capture seen by the kernel (`[input] the app holds the pointer capture`), the
//!   captured motion reaching it as `SOURCE_MOUSE_RELATIVE` (0x20004) with its relative axes, and
//!   the release seen too;
//! * the input's latency measured (`[input] ... events answered`).
//!
//! Minutes long: `cargo test -p omni-linux --release --test d7_window_input -- --ignored --nocapture`.
mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The screen position a probe motion line names (`at X,Y`).
fn at(line: &str) -> Option<(i32, i32)> {
    let rest = line.split(" at ").nth(1)?;
    let (x, rest) = rest.split_once(',')?;
    Some((x.parse().ok()?, rest.split_whitespace().next()?.parse().ok()?))
}

fn near(line: Option<&String>, want: (i32, i32), slack: i32) -> bool {
    line.and_then(|l| at(l)).is_some_and(|(x, y)| (x - want.0).abs() <= slack && (y - want.1).abs() <= slack)
}

/// The last probe line with `what` in it since `from`, if any.
fn find<'a>(lines: &'a [String], from: usize, what: &[&str]) -> Option<&'a String> {
    lines[from..].iter().find(|l| l.contains("OmniProbe") && what.iter().all(|w| l.contains(w)))
}

#[test]
#[ignore = "boots the whole system, starts an app and drives its input: minutes"]
fn the_windows_keyboard_and_mouse_reach_the_app_as_a_devices() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = std::env::temp_dir().join(format!("omni-linux-d7-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).expect("/data/local/tmp");
    let apk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/probe-app/probe.apk");
    std::fs::copy(&apk, tmp.join("probe.apk")).expect("the probe APK");
    let control = instance.with_extension("control");
    std::fs::write(&control, "").expect("the control file");
    std::env::set_var("OMNI_SCREENSHOT", instance.with_extension("png"));
    std::env::set_var("OMNI_WINDOW", "1");
    std::env::set_var("OMNI_WINDOW_CONTROL", &control);

    let then = "i=0; until [ \"$(getprop sys.boot_completed)\" = 1 ] || [ $i -ge 240 ]; do sleep 5; i=$((i+1)); done; \
                echo \"[d7] boot_completed=$(getprop sys.boot_completed)\"; \
                settings put global device_provisioned 1; settings put secure user_setup_complete 1; \
                settings put system screen_off_timeout 1800000; svc power stayon true; \
                input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard; \
                settings put global window_animation_scale 0; settings put global transition_animation_scale 0; \
                settings put global animator_duration_scale 0; settings put secure immersive_mode_confirmations confirmed; \
                pm install -r /data/local/tmp/probe.apk; echo \"[d7] pm install: $?\"; \
                am start -W -n com.omnidroid.probe/.MainActivity; echo \"[d7] am start: $?\"; \
                dumpsys input | grep -E 'omnidroid (keyboard|mouse)' | head -4 | sed 's/^/[d7] input /'";
    let mut boot = common::boot::Boot::start(&sysroot, instance, &["--zygote"], then);
    let mut lines: Vec<String> = Vec::new();
    let (mut committed, mut displayed, mut listed) = (false, false, 0);
    boot.watch(Duration::from_secs(2400), |line| {
        committed |= line.contains("OmniProbe") && line.contains("frame committed");
        displayed |= line.contains("Displayed com.omnidroid.probe/.MainActivity");
        if line.starts_with("[d7] input ") {
            listed += 1;
        }
        lines.push(line.to_string());
        committed && displayed && listed >= 2
    });
    let tail = boot.tail();
    assert!(committed && displayed, "the probe was never shown\n{tail}");
    let input_lines: Vec<&String> = lines.iter().filter(|l| l.starts_with("[d7] input ")).collect();
    assert!(input_lines.iter().any(|l| l.contains("omnidroid keyboard")) && input_lines.iter().any(|l| l.contains("omnidroid mouse")), "Android does not list both devices: {input_lines:?}\n{tail}");
    // Let the window settle on the probe.
    boot.watch(Duration::from_secs(3), |line| {
        lines.push(line.to_string());
        false
    });

    let send = |command: &str, lines: &mut Vec<String>, boot: &mut common::boot::Boot, wait: Duration, done: &dyn Fn(&[String], usize) -> bool| -> usize {
        let from = lines.len();
        let mut text = std::fs::read_to_string(&control).unwrap_or_default();
        text.push_str(command);
        text.push('\n');
        std::fs::write(&control, text).expect("the control file");
        let started = Instant::now();
        boot.watch(wait, |line| {
            lines.push(line.to_string());
            done(lines, from) || started.elapsed() > wait
        });
        from
    };

    // A key.
    let from = send("key 0x1E", &mut lines, &mut boot, Duration::from_secs(20), &|l, f| find(l, f, &["key up", "code 29"]).is_some());
    let down = find(&lines, from, &["key down", "code 29"]).cloned();
    let up = find(&lines, from, &["key up", "code 29"]).cloned();
    assert!(down.as_ref().is_some_and(|l| l.contains("scan 30") && l.contains("source 0x101")), "A's down, as a keyboard's: {down:?}\n{}", lines[from..].join("\n"));
    assert!(up.is_some(), "A's up: {}", lines[from..].join("\n"));

    // A click, exactly where it is made.
    let from = send("click 400 300", &mut lines, &mut boot, Duration::from_secs(20), &|l, f| find(l, f, &["motion ACTION_UP"]).is_some());
    let press = find(&lines, from, &["motion ACTION_DOWN"]).cloned();
    let release = find(&lines, from, &["motion ACTION_UP"]).cloned();
    assert!(
        press.as_ref().is_some_and(|l| l.contains("source 0x2002") && l.contains("tool 3") && l.contains("buttons 1")) && near(press.as_ref(), (400, 300), 1),
        "the press, as a mouse's, at (400, 300): {press:?}\n{}",
        lines[from..].iter().filter(|l| l.contains("OmniProbe")).cloned().collect::<Vec<_>>().join("\n")
    );
    assert!(near(release.as_ref(), (400, 300), 1), "the release at (400, 300): {release:?}");

    // The pointer, free: where the host's is.
    let from = send("point 500 280", &mut lines, &mut boot, Duration::from_secs(20), &|l, f| find(l, f, &["motion ACTION_HOVER_MOVE", "at 500,280"]).is_some());
    let hover = find(&lines, from, &["motion ACTION_HOVER_MOVE", "at 500,280"]).cloned();
    assert!(hover.as_ref().is_some_and(|l| l.contains("source 0x2002")), "the pointer at (500, 280): {hover:?}\n{}", lines[from..].iter().filter(|l| l.contains("OmniProbe")).cloned().collect::<Vec<_>>().join("\n"));

    // The wheel.
    let from = send("wheel 1", &mut lines, &mut boot, Duration::from_secs(20), &|l, f| find(l, f, &["motion ACTION_SCROLL"]).is_some());
    let scroll = find(&lines, from, &["motion ACTION_SCROLL"]).cloned();
    assert!(scroll.as_ref().is_some_and(|l| l.contains("vscroll 1.0")), "one notch up: {scroll:?}");
    // A second click, far from the first: the placement's gain is the display's, not one point's.
    boot.watch(Duration::from_secs(1), |line| {
        lines.push(line.to_string());
        false
    });
    let from = send("click 1000 150", &mut lines, &mut boot, Duration::from_secs(20), &|l, f| find(l, f, &["motion ACTION_UP"]).is_some());
    let press2 = find(&lines, from, &["motion ACTION_DOWN"]).cloned();
    assert!(near(press2.as_ref(), (1000, 150), 1), "the second press at (1000, 150): {press2:?}");
    eprintln!("[d7] second press: {}", press2.unwrap_or_default());

    // The app takes the pointer capture (C): the kernel sees it, and captured motion is relative.
    let capture_on = |l: &[String], f: usize| l[f..].iter().any(|l| l.contains("[input] the app holds the pointer capture")) && find(l, f, &["pointer capture on"]).is_some();
    let from = send("key 0x2E", &mut lines, &mut boot, Duration::from_secs(20), &capture_on);
    assert!(capture_on(&lines, from), "the capture taken and seen: {}", lines[from..].join("\n"));
    let from = send("move 30 -10", &mut lines, &mut boot, Duration::from_secs(20), &|l, f| find(l, f, &["captured"]).is_some());
    let captured = find(&lines, from, &["captured"]).cloned();
    assert!(captured.as_ref().is_some_and(|l| l.contains("source 0x20004") && l.contains("rel 30,-10")), "captured motion, relative: {captured:?}");
    let capture_off = |l: &[String], f: usize| l[f..].iter().any(|l| l.contains("[input] the app released the pointer capture")) && find(l, f, &["pointer capture off"]).is_some();
    let from = send("key 0x13", &mut lines, &mut boot, Duration::from_secs(20), &capture_off);
    assert!(capture_off(&lines, from), "the capture released and seen: {}", lines[from..].join("\n"));
    // Free again: the pointer is absolute. (The first hover after the capture is the window's
    // HOVER_ENTER: the dispatcher sent it a HOVER_EXIT when the capture was taken.)
    let hover_at = |l: &[String], f: usize| find(l, f, &["motion ACTION_HOVER_MOVE", "at 600,200"]).or_else(|| find(l, f, &["motion ACTION_HOVER_ENTER", "at 600,200"])).is_some();
    let from = send("point 600 200", &mut lines, &mut boot, Duration::from_secs(20), &hover_at);
    assert!(hover_at(&lines, from), "free again at (600, 200)");
    // The latency line comes every few seconds while there is input.
    let from = send("flood 1000 6", &mut lines, &mut boot, Duration::from_secs(30), &|l, f| l[f..].iter().any(|l| l.contains("events answered")));
    let latency = lines[from..].iter().find(|l| l.contains("events answered")).cloned();
    assert!(latency.is_some(), "the input's latency measured");
    eprintln!("[d7] captured: {}\n[d7] latency: {}", captured.unwrap_or_default(), latency.unwrap_or_default());
    eprintln!("[d7] key: {}\n[d7] press: {}\n[d7] hover: {}\n[d7] scroll: {}", down.unwrap_or_default(), press.unwrap_or_default(), hover.unwrap_or_default(), scroll.unwrap_or_default());
}
