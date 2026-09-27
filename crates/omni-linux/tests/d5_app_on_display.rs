//! D4 + D5: an installed APK's launcher Activity is drawn on the host display. The probe app's
//! view (one colour, 0xff2196f3) is drawn by the framework's own renderer (HWUI: EGL/GLES on ANGLE
//! on the paravirtual Vulkan driver, `crate::gpu`) into gralloc buffers of its BLASTBufferQueue,
//! committed to SurfaceFlinger in the system's host process, composed by RenderEngine and
//! presented by the host composer into the framebuffer -- whose screenshot shows the colour.
//!
//! On the way (each below the framework): socket pairs and pipes relayed between host processes
//! (vsync, input), one CLOCK_MONOTONIC per instance, gralloc regions kept while another host
//! process may open them, and binder handle references counted (a dead client's layers go).
//!
//! Minutes long: `cargo test -p omni-linux --release --test d5_app_on_display -- --ignored`.
mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The probe's view: `View.setBackgroundColor(0xff2196f3)`, as RGBA bytes.
const PROBE_BLUE: [u8; 4] = [0x21, 0x96, 0xf3, 0xff];

/// The RGBA pixel at (`x`, `y`) of the runner's screenshot (8-bit RGBA, no filters), if one is
/// there and whole.
fn pixel(png: &Path, x: usize, y: usize) -> Option<[u8; 4]> {
    let d = std::fs::read(png).ok()?;
    let (mut at, mut width, mut idat) = (8, 0usize, Vec::new());
    while at + 8 <= d.len() {
        let n = u32::from_be_bytes(d[at..at + 4].try_into().ok()?) as usize;
        let kind = &d[at + 4..at + 8];
        let body = d.get(at + 8..at + 8 + n)?;
        if kind == b"IHDR" {
            width = u32::from_be_bytes(body[0..4].try_into().ok()?) as usize;
        } else if kind == b"IDAT" {
            idat.extend_from_slice(body);
        }
        at += 12 + n;
    }
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(&idat[..]).read_to_end(&mut raw).ok()?;
    let o = y * (width * 4 + 1) + 1 + x * 4;
    raw.get(o..o + 4)?.try_into().ok()
}

#[test]
#[ignore = "boots the whole system, starts an app and waits for its frame: minutes"]
fn the_launcher_activity_is_drawn_on_the_host_display() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = std::env::temp_dir().join(format!("omni-linux-d5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).expect("/data/local/tmp");
    let apk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/probe-app/probe.apk");
    std::fs::copy(&apk, tmp.join("probe.apk")).expect("the probe APK");
    let screenshot = instance.with_extension("png");
    let _ = std::fs::remove_file(&screenshot);
    // The runner writes its display here every few seconds.
    std::env::set_var("OMNI_SCREENSHOT", &screenshot);

    // A device as its owner leaves it after setup: provisioned, awake, not locked -- and, as a
    // test device is set up (CTS, emulator test images), with window animations off: the starting
    // window's reveal is then no animation to wait on.
    let then = "i=0; until [ \"$(getprop sys.boot_completed)\" = 1 ] || [ $i -ge 240 ]; do sleep 5; i=$((i+1)); done; \
                echo \"[d5] boot_completed=$(getprop sys.boot_completed)\"; \
                settings put global device_provisioned 1; settings put secure user_setup_complete 1; \
                settings put system screen_off_timeout 1800000; svc power stayon true; \
                input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard; \
                settings put global window_animation_scale 0; settings put global transition_animation_scale 0; \
                settings put global animator_duration_scale 0; settings put secure immersive_mode_confirmations confirmed; \
                pm install -r /data/local/tmp/probe.apk; echo \"[d5] pm install: $?\"; \
                am start -W -n com.omnidroid.probe/.MainActivity; echo \"[d5] am start: $?\"; \
                sleep 30; dumpsys SurfaceFlinger --list | sed 's/^/[d5] layer /'; \
                dumpsys window | grep -E 'mCurrentFocus|isKeyguardShowing|mAwake' | sed 's/^/[d5] wm /'";
    let mut boot = common::boot::Boot::start(&sysroot, instance, &["--zygote"], then);
    let (mut committed, mut displayed, mut blue) = (false, false, false);
    let mut last_check = Instant::now();
    let mut shown_since: Option<Instant> = None;
    boot.watch(Duration::from_secs(2400), |line| {
        committed |= line.contains("OmniProbe") && line.contains("frame committed");
        displayed |= line.contains("Displayed com.omnidroid.probe/.MainActivity");
        if committed && displayed && last_check.elapsed() > Duration::from_secs(2) {
            last_check = Instant::now();
            blue = pixel(&screenshot, 640, 360) == Some(PROBE_BLUE);
        }
        // Three minutes after the frame was committed, the display has shown it or never will.
        let since = if committed && displayed { *shown_since.get_or_insert_with(Instant::now) } else { Instant::now() };
        blue || (committed && displayed && since.elapsed() > Duration::from_secs(180))
    });
    let tail = boot.tail();
    assert!(committed, "the probe's first frame was never committed to SurfaceFlinger\n{tail}");
    assert!(displayed, "the window manager never reported the probe's Activity displayed\n{tail}");
    assert!(
        blue,
        "the host display never showed the probe's view (centre pixel {:02x?}, screenshot {})\n{tail}",
        pixel(&screenshot, 640, 360),
        screenshot.display()
    );
}
