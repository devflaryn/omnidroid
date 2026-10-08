//! **What showing a frame in the display window costs the system host process's CPU**, by each
//! present path (`present_bgra`, `present_gpu`): a real window, a real swapchain, 1575x890 frames at
//! 60 a second, the process's CPU time (`GetProcessTimes`: every thread -- the presenting one, the
//! window's `WM_PAINT`, the driver's) per frame. Rounds interleaved, each path four times; the
//! median round is reported. Twice: the frame at 1:1 in its window, and stretched to a 2400x1300
//! window (a maximised window on a 2560x1440 screen gets a display capped at 1600x900's pixels).
//!
//! - **gdi rgba** (the default): the frame swizzled into the window's canvas, `StretchDIBits` from
//!   `WM_PAINT`;
//! - **gdi bgra** (`present_bgra=1`): the canvas takes the frame shared, `StretchDIBits`;
//! - **gpu** (`present_gpu=1`): a copy into the swapchain presenter's host-visible memory, the GPU
//!   copies and blits it, MAILBOX present.
//!
//! A measurement, not a check, and it opens a window (as the window tests do):
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 cargo test --release -p omni-linux --test present_cost -- --ignored --nocapture
//! ```
#![cfg(windows)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_linux::gpu::window_present::WindowPresenter;
use omni_platform::window::{Window, WindowDesc};

const NAMES: [&str; 3] = ["gdi rgba (default)", "gdi bgra (present_bgra=1)", "gpu (present_gpu=1)"];
const N: u32 = 180;
const PERIOD: Duration = Duration::from_micros(16_667);

/// Pump the window for a moment (its opening, a resize).
fn settle(window: &mut Window) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        window.poll_events().for_each(drop);
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The frames, RGBA and BGRA (three of each, so nothing is presented twice in a row).
struct Frames {
    w: u32,
    h: u32,
    rgba: Vec<Vec<u8>>,
    bgra: Vec<Arc<Vec<u8>>>,
}

/// One round of `path`: process CPU ms a frame and the present call's ms.
fn round(window: &mut Window, frames: &Frames, path: usize, gpu: &mut Option<WindowPresenter>) -> (f64, f64) {
    let presenter = window.presenter();
    let client = window.client_size().expect("its size");
    let mut one = |i: u32, window: &mut Window| {
        let f = (i % 3) as usize;
        let t = Instant::now();
        match path {
            0 => presenter.present_rgba(&frames.rgba[f], frames.w, frames.h).expect("present"),
            1 => presenter.present_bgra(Arc::clone(&frames.bgra[f]), frames.w, frames.h).expect("present"),
            _ => {
                gpu.as_mut().expect("made").present(&frames.rgba[f], frames.w, frames.h, false, client).expect("present");
            }
        }
        let spent = t.elapsed();
        window.poll_events().for_each(drop);
        spent
    };
    // A few frames first: a new swapchain, a canvas growing to size.
    for i in 0..10 {
        one(i, window);
        std::thread::sleep(PERIOD);
    }
    let cpu0 = omni_platform::process::cpu_time().expect("cpu time");
    let (start, mut calls) = (Instant::now(), Duration::ZERO);
    for i in 0..N {
        calls += one(i, window);
        if let Some(wait) = (start + PERIOD * (i + 1)).checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    let used = omni_platform::process::cpu_time().expect("cpu time") - cpu0;
    (used.as_secs_f64() * 1000.0 / f64::from(N), calls.as_secs_f64() * 1000.0 / f64::from(N))
}

#[test]
#[ignore = "a measurement that opens a window: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn present_cost() {
    assert!(std::env::var("OMNI_GFX_WINDOW_TESTS").as_deref() == Ok("1"), "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1");
    let mut window = Window::new(&WindowDesc::new("omni present_cost", 1575, 890)).expect("a window");
    window.show();
    settle(&mut window);
    let (w, h) = window.client_size().expect("its size");
    let (wu, hu) = (w as usize, h as usize);
    let rgba: Vec<Vec<u8>> = (0..3usize).map(|k| (0..wu * hu).flat_map(|i| [((i + 7 * k) % 251) as u8, ((i / wu + k) % 241) as u8, (i % 239) as u8, 255]).collect()).collect();
    let bgra = rgba.iter().map(|f| Arc::new(f.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect())).collect();
    let frames = Frames { w, h, rgba, bgra };
    let raw = window.raw();
    for scaled in [None, Some((2400u32, 1300u32))] {
        if let Some((sw, sh)) = scaled {
            window.set_client_size(sw, sh).expect("resize");
            settle(&mut window);
        }
        let client = window.client_size().expect("its size");
        let (mut cpu, mut call): ([Vec<f64>; 3], [Vec<f64>; 3]) = Default::default();
        for r in 0..4usize {
            for k in 0..3usize {
                let path = (r + k) % 3;
                let mut gpu = (path == 2).then(|| {
                    window.presenter().clear();
                    WindowPresenter::new(raw).expect("a swapchain on the window")
                });
                if r == 0 {
                    if let Some(g) = &gpu {
                        eprintln!("present_cost: gpu path on {}", g.describe());
                    }
                }
                let (c, p) = round(&mut window, &frames, path, &mut gpu);
                cpu[path].push(c);
                call[path].push(p);
            }
        }
        let med = |v: &[f64]| {
            let mut v = v.to_vec();
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        for p in 0..3 {
            eprintln!(
                "present_cost frame {w}x{h} in window {}x{} {}: process CPU {:.2} ms/frame (rounds {:.2?}); the present call {:.2} ms",
                client.0,
                client.1,
                NAMES[p],
                med(&cpu[p]),
                cpu[p],
                med(&call[p])
            );
        }
    }
}
