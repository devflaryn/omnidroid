//! **What reaches the screen**: a frame presented through MoltenVK to a real window, captured by
//! the window server, and its colour checked.
//!
//! `omni-gfx/tests/host_present_live.rs` reads the swapchain image back, which is exactly what was
//! handed to the presentation engine and one step short of the display. This is that step: the
//! window's composited image as `screencapture -l <window>` takes it from the window server.
//!
//! # What this capture can and cannot see (VERIFICATION entry 19)
//!
//! Entry 19 is a Windows capture (`PrintWindow`) that could not see a flip-model swapchain and
//! reported the window class's background instead. So this capture is first **shown to see
//! something known**: two frames of two different colours must come back as two different captures
//! matching each colour, and a capture that returned a default, a background or a stale frame
//! would fail the second. What it cannot see: anything the window server composites differently
//! for the display than for a capture (it captures the window's own surface, not the monitor), and
//! it needs Screen Recording permission for the process running it -- MEASURED granted on this host
//! (`CGPreflightScreenCaptureAccess` true); without it, macOS returns the desktop picture in place
//! of other windows, which this test would report as a colour mismatch, not as a pass.
//!
//! Colours are compared with a tolerance: the capture is colour-managed from the layer's colour
//! space to the capture's, so 8-bit values move by a few steps. The measured values are printed.
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-platform --release --test screen_capture_macos -- --ignored --nocapture
//! ```

#![cfg(target_os = "macos")]

use core::ffi::c_void;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use objc2::msg_send;
use objc2::runtime::AnyObject;
use omni_gfx::vulkan::{FrameOutcome, Renderer, RendererConfig};
use omni_platform::window::{RawWindow, Window, WindowDesc, WindowEvent};

#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}
#[allow(non_upper_case_globals)]
unsafe extern "C" {
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_sync_f(queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

/// `-[NSWindow windowNumber]`, asked on the main thread.
fn window_number(ns_window: isize) -> isize {
    struct Job(isize, isize);
    extern "C" fn run(context: *mut c_void) {
        let job = unsafe { &mut *context.cast::<Job>() };
        let window = unsafe { &*(job.0 as *const AnyObject) };
        job.1 = unsafe { msg_send![window, windowNumber] };
    }
    let mut job = Job(ns_window, 0);
    unsafe { dispatch_sync_f(&raw const _dispatch_main_q, (&raw mut job).cast(), run) };
    job.1
}

/// Capture window `number` with `screencapture`, convert with `sips`, and answer the BGR pixel at
/// fractions `(fx, fy)` of the image, plus the image's size.
fn capture(number: isize, dir: &Path, name: &str, fx: f64, fy: f64) -> ([u8; 3], (u32, u32)) {
    let png = dir.join(format!("{name}.png"));
    let bmp = dir.join(format!("{name}.bmp"));
    let shot = Command::new("screencapture").arg("-x").arg("-o").arg(format!("-l{number}")).arg(&png).status().unwrap();
    assert!(shot.success(), "screencapture failed: {shot:?}");
    let convert = Command::new("sips").args(["-s", "format", "bmp"]).arg(&png).arg("--out").arg(&bmp).output().unwrap();
    assert!(convert.status.success(), "sips failed: {convert:?}");
    let data = std::fs::read(&bmp).unwrap();
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    let offset = u32_at(10) as usize;
    let width = u32_at(18);
    let raw_height = u32_at(22) as i32;
    let bits = u16::from_le_bytes(data[28..30].try_into().unwrap()) as usize;
    assert!(bits == 24 || bits == 32, "a {bits}-bit BMP");
    let height = raw_height.unsigned_abs();
    let stride = (width as usize * bits).div_ceil(32) * 4;
    let x = (f64::from(width) * fx) as usize;
    let y = (f64::from(height) * fy) as usize;
    // Positive height: rows are stored bottom-up.
    let row = if raw_height > 0 { height as usize - 1 - y } else { y };
    let at = offset + row * stride + x * bits / 8;
    ([data[at], data[at + 1], data[at + 2]], (width, height))
}

fn close_to(got: [u8; 3], want: [u8; 3], tolerance: u8) -> bool {
    got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= tolerance)
}

#[test]
#[ignore = "needs a desktop session, a GPU and Screen Recording permission: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_presented_colour_is_what_the_window_server_captures() {
    assert!(
        std::env::var("OMNI_GFX_WINDOW_TESTS").is_ok_and(|v| v == "1"),
        "run with --ignored but OMNI_GFX_WINDOW_TESTS is not 1; this opens a window and captures it"
    );
    let mut window = Window::new(&WindowDesc::new("omnidroid: screen capture", 320, 240)).unwrap();
    window.show();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if window.poll_events().any(|e| e == WindowEvent::FocusChanged { focused: true }) {
            break;
        }
        assert!(Instant::now() < deadline, "the window never took the focus");
        window.wait(Duration::from_millis(50));
    }
    let RawWindow::AppKit { ns_window, .. } = window.raw() else { panic!("{:?}", window.raw()) };
    let number = window_number(ns_window);
    let mut renderer = Renderer::new(window.raw(), window.client_size().unwrap(), RendererConfig::default()).unwrap();
    let dir = std::env::temp_dir().join(format!("omni-capture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // (RGBA cleared, BGR expected in the capture)
    let cases = [([0.8f32, 0.1, 0.1, 1.0], [26u8, 26, 204]), ([0.1, 0.2, 0.9, 1.0], [230, 51, 26])];
    let mut seen = Vec::new();
    for (index, (rgba, bgr)) in cases.iter().enumerate() {
        // Several frames, so that the window server has composited one of this colour.
        let mut presented = 0;
        while presented < 10 {
            let _ = window.poll_events().count();
            if renderer.present_clear(*rgba).unwrap() == FrameOutcome::Presented {
                presented += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
        // Below the title bar: the lower middle of the captured window.
        let (pixel, size) = capture(number, &dir, &format!("case{index}"), 0.5, 0.7);
        println!("cleared to {rgba:?}: captured BGR {pixel:?} (expected about {bgr:?}) in a {size:?} capture");
        seen.push(pixel);
        assert!(close_to(pixel, *bgr, 24), "the capture shows {pixel:?} where {bgr:?} was presented");
    }
    assert_ne!(seen[0], seen[1], "two colours presented, one capture: the capture is not seeing the frames");
    drop(renderer);
    drop(window);
    let _ = std::fs::remove_dir_all(dir);
}
