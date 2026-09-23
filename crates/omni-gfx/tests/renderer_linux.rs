//! **The presented pixels, read back from the X server's framebuffer** -- the capture
//! `renderer_live.rs` says it cannot make on Windows, made on Linux.
//!
//! On Windows `PrintWindow` does not see a flip-model swapchain (VERIFICATION entry 19). Under X11
//! the swapchain's images end up in the window's pixels on the server -- on Mesa's software
//! presentation path, which is lavapipe's, literally by `PutImage` -- and `import -window root`
//! (ImageMagick, a separate program with its own connection) reads the root window's composed
//! pixels exactly where the window is on screen, which `xwininfo` (a third) reports.
//!
//! **The instrument is shown seeing a known colour first** (entry 19's rule): the root window is
//! painted a colour with `xsetroot`, which has nothing to do with this crate, and a capture of an
//! empty part of the screen must return that colour before a capture of the window is allowed to
//! say anything about the renderer.
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -p omni-gfx --release --test renderer_linux \
//!     -- --ignored --test-threads=1
//! ```
//!
//! Needs an X server with no window manager over the window (the port's Xvfb `:92`), because the
//! capture reads the screen and a frame or a shadow would be pixels too. **Lavapipe caveat**: the
//! only Vulkan device on the port's machine is Mesa's CPU rasteriser, so this proves the pixels,
//! not the speed.

#![cfg(target_os = "linux")]

use std::process::Command;
use std::time::{Duration, Instant};

use omni_gfx::Rgba8Image;
use omni_gfx::vulkan::{FrameOutcome, Renderer, RendererConfig};
use omni_platform::window::{RawWindow, Window, WindowDesc, WindowEvent};

const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

fn require_gate() {
    assert!(
        std::env::var(GATE).is_ok_and(|v| v == "1"),
        "run with --ignored but {GATE} is not 1; this opens a window and presents to it"
    );
    assert!(std::env::var("DISPLAY").is_ok_and(|d| !d.is_empty()), "{GATE}=1 but $DISPLAY is unset");
}

/// Run a program and return its standard output, failing the test when it fails.
fn run(program: &str, args: &[&str]) -> Vec<u8> {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("{program} could not be run: {err}"));
    assert!(output.status.success(), "{program} {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    output.stdout
}

/// The RGB pixels of the screen rectangle `width` x `height` at (`x`, `y`), row by row, as
/// ImageMagick's `import` reads them from the server.
fn capture(x: i32, y: i32, width: u32, height: u32) -> Vec<[u8; 3]> {
    let crop = format!("{width}x{height}+{x}+{y}");
    let bytes = run("import", &["-window", "root", "-crop", &crop, "+repage", "-depth", "8", "rgb:-"]);
    assert_eq!(bytes.len(), (width * height * 3) as usize, "import returned {} bytes for {crop}", bytes.len());
    bytes.chunks_exact(3).map(|p| [p[0], p[1], p[2]]).collect()
}

/// Where `window`'s top-left corner is on the screen, as `xwininfo` reports it.
fn origin(window: &Window) -> (i32, i32) {
    let RawWindow::Xlib { window: id, .. } = window.raw() else {
        panic!("the Linux window must be an Xlib window: {:?}", window.raw())
    };
    let info = String::from_utf8(run("xwininfo", &["-id", &id.to_string()])).unwrap();
    let field = |name: &str| -> i32 {
        info.lines()
            .find_map(|line| line.trim().strip_prefix(name))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_else(|| panic!("xwininfo gave no {name:?}: {info}"))
    };
    (field("Absolute upper-left X:"), field("Absolute upper-left Y:"))
}

/// Present frames with `present` until a capture of the window's client area satisfies `seen`,
/// or fail after 10 s with what the capture last saw. Frames are presented between captures so
/// that a present path that shows its frame late still gets there.
fn present_until(
    window: &mut Window,
    renderer: &mut Renderer,
    what: &str,
    mut present: impl FnMut(&mut Renderer) -> FrameOutcome,
    seen: impl Fn(&[[u8; 3]]) -> bool,
) -> Vec<[u8; 3]> {
    let (width, height) = window.client_size().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        for event in window.poll_events() {
            if let WindowEvent::Resized { width, height } = event {
                renderer.notify_resized(width, height);
            }
        }
        present(renderer);
        let (x, y) = origin(window);
        let pixels = capture(x, y, width, height);
        if seen(&pixels) {
            return pixels;
        }
        assert!(Instant::now() < deadline, "10 s and the screen never showed {what}; it showed {:?}", summary(&pixels));
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The distinct colours of a capture and how many pixels each has, most common first.
fn summary(pixels: &[[u8; 3]]) -> Vec<([u8; 3], usize)> {
    let mut counts = std::collections::HashMap::new();
    for pixel in pixels {
        *counts.entry(*pixel).or_insert(0usize) += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    counts.truncate(8);
    counts
}

/// **Cleared frames and an uploaded image, seen on the screen.** The capture first proves it can
/// see a colour nobody in this process drew; then the window shows red, then green -- every pixel
/// of the client area, exactly -- and then a four-quadrant RGBA8 image, each quadrant its own
/// colour at its own place, blitted one to one.
#[test]
#[ignore = "needs an X server and a Vulkan driver: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn the_presented_pixels_are_on_the_screen_where_the_window_is() {
    require_gate();
    // The window first, unmapped: an Xvfb resets -- root background included -- when its last
    // client disconnects, so the root is painted while this process holds a connection.
    let (width, height) = (320u32, 240u32);
    let mut window = Window::new(&WindowDesc::new("omnidroid: pixels", width, height)).unwrap();
    let _ = window.poll_events().count();

    // The instrument, on a known answer: the root painted by another program.
    let root = [0x33, 0x66, 0xCC];
    run("xsetroot", &["-solid", "#3366CC"]);
    let empty = capture(1700, 900, 64, 64);
    assert!(empty.iter().all(|p| *p == root), "the capture cannot see the root colour: {:?}", summary(&empty));

    let mut renderer = Renderer::new(window.raw(), window.client_size().unwrap(), RendererConfig::default())
        .unwrap_or_else(|err| panic!("could not create the renderer: {err}"));
    window.show();

    for (name, clear, rgb) in [("red", [1.0, 0.0, 0.0, 1.0], [255, 0, 0]), ("green", [0.0, 1.0, 0.0, 1.0], [0, 255, 0])] {
        present_until(
            &mut window,
            &mut renderer,
            name,
            |r| r.present_clear(clear).unwrap(),
            |pixels| pixels.iter().all(|p| *p == rgb),
        );
    }

    // Four quadrants, the image the size of the window so the blit is one to one.
    let quadrant = |x: u32, y: u32| -> [u8; 4] {
        match (x < width / 2, y < height / 2) {
            (true, true) => [255, 0, 0, 255],
            (false, true) => [0, 255, 0, 255],
            (true, false) => [0, 0, 255, 255],
            (false, false) => [255, 255, 255, 255],
        }
    };
    let mut bytes = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            bytes.extend_from_slice(&quadrant(x, y));
        }
    }
    let image = Rgba8Image::new(width, height, &bytes).unwrap();
    let pixels = present_until(
        &mut window,
        &mut renderer,
        "the four quadrants",
        |r| r.present_rgba8(&image).unwrap(),
        |pixels| {
            (0..height).all(|y| {
                (0..width).all(|x| {
                    let want = quadrant(x, y);
                    pixels[(y * width + x) as usize] == [want[0], want[1], want[2]]
                })
            })
        },
    );
    assert_eq!(summary(&pixels).len(), 4, "exactly the four colours: {:?}", summary(&pixels));

    // And the window painted only itself: the root beside it is still the root.
    let (x, y) = origin(&window);
    let beside = capture(x + width as i32 + 8, y, 16, 16);
    assert!(beside.iter().all(|p| *p == root), "outside the window: {:?}", summary(&beside));
}
