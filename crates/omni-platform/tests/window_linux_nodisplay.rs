//! **No X server is a typed refusal naming the display**, not a crash and not a default: the Linux
//! side of `window_seam.rs`'s "a valid description reaches the backend", which on Linux is now a
//! real backend that has to be told where the server is.
//!
//! Its own test binary because it rewrites `$DISPLAY`, which is process-wide. Needs no X server
//! -- needing one's *absence* is the point -- so it runs in every `cargo test`.

#![cfg(target_os = "linux")]

use omni_platform::window::{Window, WindowDesc, WindowError};

#[test]
fn a_display_that_does_not_exist_is_refused_naming_it_and_xopendisplay() {
    // A display number nothing listens on: X sockets are /tmp/.X11-unix/X<n>.
    let display = ":187";
    assert!(
        !std::path::Path::new("/tmp/.X11-unix/X187").exists(),
        "something is listening on {display}; this test needs a display number nobody uses"
    );
    std::env::set_var("DISPLAY", display);
    std::env::remove_var("WAYLAND_DISPLAY");
    let error = Window::new(&WindowDesc::new("omnidroid", 640, 480)).unwrap_err();
    match &error {
        WindowError::X11 { operation, api, detail } => {
            assert_eq!(*operation, "create");
            assert_eq!(*api, "XOpenDisplay");
            assert!(detail.contains(display), "the refusal names the display: {detail}");
        }
        other => panic!("expected the X11 refusal, got {other:?}"),
    }
    assert!(!error.is_unsupported(), "Linux has a backend; this is the host saying no: {error}");

    // And a Wayland-only session is told why X is still what is needed.
    std::env::set_var("WAYLAND_DISPLAY", "wayland-187");
    let error = Window::new(&WindowDesc::new("omnidroid", 640, 480)).unwrap_err().to_string();
    assert!(error.contains("Xwayland") && error.contains("wayland-187"), "{error}");
}
