//! D7: the live window's keyboard and mouse reach the app as a device's keyboard and mouse.
//!
//! The runner shows the display in its window with input on (`OMNI_WINDOW=1`,
//! `display_window`/`window_input`): `/dev/input/event0` a keyboard, `event1` a mouse, read by
//! system_server's own `InputReader`. Through the runner's control file -- the same translation the
//! window's own events take -- the gate presses A, clicks at (400, 300), waits, moves the mouse by
//! (+50, -20) and turns the wheel a notch. The probe app (`tests/fixtures/probe-app`) logs what
//! reaches it, and the gate requires:
//!
//! * Android lists both devices (`dumpsys input`: `omnidroid keyboard`, `omnidroid mouse`);
//! * the key as a keyboard's: `KEYCODE_A` (29), scan code `KEY_A` (30), source `SOURCE_KEYBOARD`;
//! * the click as a mouse's, **where it was made**: `ACTION_DOWN` from `SOURCE_MOUSE` (0x2002),
//!   tool `TOOL_TYPE_MOUSE`, primary button, at (400, 300) within a pixel -- the placement a first
//!   click makes (home, then one move over `window_input::PLACE_GAIN`) -- and its `ACTION_UP`; and
//!   a second at (1000, 150), so the gain holds across the display, not at one point;
//! * a move of (+50, -20) counts after a rest: a hover where Android's first-move gain puts it;
//! * the wheel: an `ACTION_SCROLL` of one notch up.
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

    // A click where it is made: placed (home, then one move), then pressed.
    let from = send("click 400 300", &mut lines, &mut boot, Duration::from_secs(20), &|l, f| find(l, f, &["motion ACTION_UP"]).is_some());
    let press = find(&lines, from, &["motion ACTION_DOWN"]).cloned();
    let release = find(&lines, from, &["motion ACTION_UP"]).cloned();
    assert!(
        press.as_ref().is_some_and(|l| l.contains("source 0x2002") && l.contains("tool 3") && l.contains("buttons 1")) && near(press.as_ref(), (400, 300), 1),
        "the press, as a mouse's, at (400, 300): {press:?}\n{}",
        lines[from..].iter().filter(|l| l.contains("OmniProbe")).cloned().collect::<Vec<_>>().join("\n")
    );
    assert!(near(release.as_ref(), (400, 300), 1), "the release at (400, 300): {release:?}");

    // A move after a rest: unaccelerated, so exactly where it was sent.
    boot.watch(Duration::from_secs(1), |line| {
        lines.push(line.to_string());
        false
    });
    let from = send("move 50 -20", &mut lines, &mut boot, Duration::from_secs(20), &|l, f| find(l, f, &["motion ACTION_HOVER_MOVE"]).is_some());
    let hover = find(&lines, from, &["motion ACTION_HOVER_MOVE"]).cloned();
    let gain = omni_linux::window_input::PLACE_GAIN;
    let expect = (400 + (50.0 * gain).round() as i32, 300 - (20.0 * gain).round() as i32);
    assert!(hover.as_ref().is_some_and(|l| l.contains("source 0x2002")) && near(hover.as_ref(), expect, 2), "the move to {expect:?}: {hover:?}");

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
    eprintln!("[d7] key: {}\n[d7] press: {}\n[d7] hover: {}\n[d7] scroll: {}", down.unwrap_or_default(), press.unwrap_or_default(), hover.unwrap_or_default(), scroll.unwrap_or_default());
}
