//! `Window::present_rgba` on a **real** Win32 window: the image is on the window, stretched to its
//! client area, and it is still there -- stretched to the new size -- after a resize that nobody
//! presented after (the backend's `WM_PAINT` of its kept image). Read back from the window's own DC
//! with `GetPixel`, so what is asserted is what GDI painted, not what the backend meant to.
//!
//! Colours are compared by **which channel dominates**, not byte for byte: MEASURED on this host
//! (2026-09-28), a pixel painted 0xe01020 reads back as 0xf1256b and 0x2196f3 as 0x0faaff -- the
//! display's colour management applies to the read-back -- so an exact comparison would test the
//! monitor's profile. Red and blue stay unmistakable.
//!
//! What it does **not** isolate: the backend's `InvalidateRect` on `WM_SIZE`. With that line
//! removed this test still passes (MEASURED, 2026-09-28), because the programmatic resize
//! (`SetWindowPos`) invalidates the whole client area on this host by itself; the line is for a
//! user's border drag, which no test can make.
//!
//! Gated as `window_live.rs` is (VERIFICATION entry 4: asked for and unable to run is a failure):
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-platform --release --test window_present_windows -- --ignored
//! ```
#![cfg(windows)]

use std::time::{Duration, Instant};

use omni_platform::window::{RawWindow, Window, WindowDesc, WindowError, WindowEvent};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC};

const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

fn require_gate() {
    assert!(
        std::env::var(GATE).is_ok_and(|v| v == "1"),
        "run with --ignored but {GATE} is not 1: this test creates a real window and needs a desktop session"
    );
}

/// The window's pixel at client (`x`, `y`) as `[r, g, b]`, from its own DC.
fn pixel(window: &Window, x: i32, y: i32) -> [u8; 3] {
    let RawWindow::Win32 { hwnd, .. } = window.raw() else { panic!("not a Win32 window") };
    let hwnd = hwnd as HWND;
    // SAFETY: a live window of this thread; the DC is released below.
    unsafe {
        let dc = GetDC(hwnd);
        let c = GetPixel(dc, x, y);
        ReleaseDC(hwnd, dc);
        [(c & 0xff) as u8, (c >> 8 & 0xff) as u8, (c >> 16 & 0xff) as u8]
    }
}

/// Pump the window until `done` holds or two seconds pass.
fn pump_until(window: &mut Window, mut done: impl FnMut(&Window) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        window.poll_events().for_each(drop);
        if done(window) {
            return true;
        }
        window.wait(Duration::from_millis(20));
    }
    false
}

const RED: [u8; 4] = [0xe0, 0x10, 0x20, 0xff];
const BLUE: [u8; 4] = [0x21, 0x96, 0xf3, 0x00];

#[derive(Debug, PartialEq, Eq)]
enum Hue {
    Red,
    Blue,
    Other,
}

/// Which of the two test colours `p` is, allowing for the display's colour management.
fn hue(p: [u8; 3]) -> Hue {
    let [r, g, b] = p.map(i32::from);
    if r > 0xb0 && r > g + 0x60 && r > b + 0x40 {
        Hue::Red
    } else if b > 0xc0 && b > r + 0x60 {
        Hue::Blue
    } else {
        Hue::Other
    }
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_presented_image_fills_the_client_area_and_follows_a_resize() {
    require_gate();
    let mut window = Window::new(&WindowDesc::new("omni present_rgba", 200, 100)).expect("a window");
    window.show();
    let (w, h) = window.client_size().expect("its size");
    // Two pixels: red on the left, blue on the right (its alpha is 0 and must not matter).
    let image: Vec<u8> = RED.iter().chain(BLUE.iter()).copied().collect();
    window.present_rgba(&image, 2, 1).expect("present");
    // Middles and corners: the image is stretched to the whole client area, not placed 1:1. Polled
    // until all agree, because what `GetPixel` reads here is the composed screen (the colour
    // managed values above), which the window's opening animation scales and blends for a moment.
    let points = |w: u32, h: u32| {
        let (w, h) = (w as i32, h as i32);
        [(w / 4, h / 2, Hue::Red), (w * 3 / 4, h / 2, Hue::Blue), (0, 0, Hue::Red), (1, h - 2, Hue::Red), (w - 1, h - 1, Hue::Blue), (w - 2, 1, Hue::Blue)]
    };
    let wrong = |win: &Window, w: u32, h: u32| -> Vec<(i32, i32, [u8; 3])> {
        points(w, h).into_iter().filter(|(x, y, want)| hue(pixel(win, *x, *y)) != *want).map(|(x, y, _)| (x, y, pixel(win, x, y))).collect()
    };
    assert!(pump_until(&mut window, |win| wrong(win, w, h).is_empty()), "the image is not on the {w}x{h} window: {:02x?}", wrong(&window, w, h));

    // Grown, and nothing presented since: the window repaints the kept image at its new size.
    window.set_client_size(400, 220).expect("resize");
    let (w2, h2) = window.client_size().expect("its new size");
    assert!(w2 > w && h2 > h, "the window did not grow: {w}x{h} -> {w2}x{h2}");
    assert!(
        pump_until(&mut window, |win| wrong(win, w2, h2).is_empty()),
        "after a resize the kept image was not repainted over the {w2}x{h2} window: {:02x?}",
        wrong(&window, w2, h2)
    );
    // The resize is still reported: presenting does not swallow the window's events.
    window.set_client_size(300, 150).expect("resize");
    let (w3, h3) = window.client_size().expect("size");
    let events: Vec<WindowEvent> = window.poll_events().collect();
    assert!(events.contains(&WindowEvent::Resized { width: w3, height: h3 }), "no Resized to {w3}x{h3} in {events:?}");
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_short_buffer_or_an_empty_image_is_refused_by_name() {
    require_gate();
    let mut window = Window::new(&WindowDesc::new("omni present_rgba refusals", 120, 80)).expect("a window");
    let short = window.present_rgba(&[0; 7], 1, 2).expect_err("7 bytes for 2 pixels");
    assert!(matches!(short, WindowError::PixelsTooShort { needed: 8, got: 7, .. }), "{short:?}");
    let empty = window.present_rgba(&[], 0, 5).expect_err("a zero-width image");
    assert!(matches!(empty, WindowError::SizeOutOfRange { width: 0, .. }), "{empty:?}");
    // And the window's events are unaffected by the refusals.
    assert!(window.poll_events().any(|e| matches!(e, WindowEvent::Resized { .. })), "the creation's size is still reported");
}

/// **A `Presenter` on another thread**: its image reaches the window, painted by the window's own
/// thread as it pumps; its `client_size` answers the size a resize made; and once the window is
/// dropped it presents nothing and says so with `None`.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_presenter_on_another_thread_presents_and_follows_the_size() {
    require_gate();
    let mut window = Window::new(&WindowDesc::new("omni presenter", 240, 120)).expect("a window");
    window.show();
    let presenter = window.presenter();
    let image: Vec<u8> = RED.iter().chain(BLUE.iter()).copied().collect();
    let from = std::thread::spawn({
        let presenter = presenter.clone();
        move || presenter.present_rgba(&image, 2, 1)
    });
    from.join().expect("the presenting thread").expect("present");
    let (w, h) = window.client_size().expect("size");
    assert!(
        pump_until(&mut window, |win| hue(pixel(win, (w / 4) as i32, (h / 2) as i32)) == Hue::Red && hue(pixel(win, (w * 3 / 4) as i32, (h / 2) as i32)) == Hue::Blue),
        "the other thread's image is not on the window"
    );
    window.set_client_size(317, 141).expect("resize");
    let asked = std::thread::spawn({
        let presenter = presenter.clone();
        move || presenter.client_size()
    });
    assert_eq!(asked.join().expect("the asking thread"), Some(window.client_size().expect("size")));
    drop(window);
    assert_eq!(presenter.client_size(), None, "a presenter outliving its window has no size");
    presenter.present_rgba(&[0; 4], 1, 1).expect("a present to a closed window is dropped, not refused");
}

/// **`Presenter::present_bgra`**: a BGRA image handed over shared is painted as its RGBA twin is
/// (red left, blue right), and a short one is refused by name.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_bgra_image_is_painted_as_its_rgba_twin() {
    require_gate();
    let mut window = Window::new(&WindowDesc::new("omni present_bgra", 240, 120)).expect("a window");
    window.show();
    let presenter = window.presenter();
    let bgra: Vec<u8> = [RED, BLUE].iter().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    presenter.present_bgra(std::sync::Arc::new(bgra), 2, 1).expect("present");
    let (w, h) = window.client_size().expect("size");
    assert!(
        pump_until(&mut window, |win| hue(pixel(win, (w / 4) as i32, (h / 2) as i32)) == Hue::Red && hue(pixel(win, (w * 3 / 4) as i32, (h / 2) as i32)) == Hue::Blue),
        "the BGRA image is not on the window as its RGBA twin"
    );
    let short = presenter.present_bgra(std::sync::Arc::new(vec![0; 7]), 1, 2).expect_err("7 bytes for 2 pixels");
    assert!(matches!(short, WindowError::PixelsTooShort { needed: 8, got: 7, .. }), "{short:?}");
}
