//! **The renderer under a real X window manager**: minimising, which on X11 only a manager can do
//! (ICCCM gives it iconic state), with every window event the renderer was fed kept for the
//! failure message.
//!
//! `renderer_live.rs`'s minimise test asserts the same outcome; this one exists because on X11 the
//! outcome depends on two things that test cannot show -- which window events arrived, and in what
//! order relative to the presents -- and a failure here says both.
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:93 cargo test -p omni-gfx --release --test renderer_linux_wm \
//!     -- --ignored --test-threads=1
//! ```
//!
//! On the port's Xvfb `:93`, with `xfwm4 --compositor=off` running. Lavapipe caveat as for every
//! graphics figure here: correctness, not speed.

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use omni_gfx::vulkan::{FrameOutcome, Renderer, RendererConfig};
use omni_platform::window::{Window, WindowDesc, WindowEvent};

const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

/// Present cleared frames, feeding every resize to the renderer, until `done`; fail after 10 s
/// with the events and outcomes seen.
fn drive(
    window: &mut Window,
    renderer: &mut Renderer,
    events: &mut Vec<WindowEvent>,
    what: &str,
    done: impl Fn(&Renderer) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut outcomes = Vec::new();
    loop {
        for event in window.poll_events() {
            if let WindowEvent::Resized { width, height } = event {
                renderer.notify_resized(width, height);
            }
            events.push(event);
        }
        let outcome = renderer.present_clear([0.1, 0.2, 0.3, 1.0]).unwrap();
        if outcomes.last().is_none_or(|(last, _)| *last != outcome) {
            outcomes.push((outcome, 1));
        } else if let Some((_, count)) = outcomes.last_mut() {
            *count += 1;
        }
        if done(renderer) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "10 s waiting for {what}: the renderer is {renderer:?}; the window events were \
             {events:?}; the outcomes, run-length, were {outcomes:?}"
        );
    }
}

/// **Minimised: no swapchain, frames skipped; restored: a swapchain at the window's size.** The
/// minimise is asked for straight after the first frame, while the manager may still be taking
/// the window on -- the case `set_minimized` has to keep for it.
#[test]
#[ignore = "needs an X server with a window manager: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:93"]
fn a_minimised_window_has_no_swapchain_and_a_restored_one_gets_it_back() {
    assert!(std::env::var(GATE).is_ok_and(|v| v == "1"), "run with --ignored but {GATE} is not 1");
    let mut window = Window::new(&WindowDesc::new("omnidroid: wm minimise", 640, 480)).unwrap();
    let _ = window.poll_events().count();
    let mut renderer = Renderer::new(window.raw(), window.client_size().unwrap(), RendererConfig::default())
        .unwrap_or_else(|err| panic!("could not create the renderer: {err}"));
    let mut events = Vec::new();
    window.show();
    drive(&mut window, &mut renderer, &mut events, "the first frame", |r| r.frames_presented() >= 1);

    window.set_minimized(true).unwrap();
    drive(&mut window, &mut renderer, &mut events, "no swapchain", |r| r.swapchain_extent().is_none());
    assert!(events.contains(&WindowEvent::Resized { width: 0, height: 0 }), "{events:?}");
    assert_eq!(renderer.present_clear([0.0, 0.0, 0.0, 1.0]).unwrap(), FrameOutcome::Skipped);
    assert_eq!(window.client_size().unwrap(), (0, 0));

    window.set_minimized(false).unwrap();
    let before = renderer.frames_presented();
    drive(&mut window, &mut renderer, &mut events, "a frame after the restore", |r| r.frames_presented() > before);
    assert_eq!(renderer.swapchain_extent(), Some((640, 480)), "{events:?}");
}
