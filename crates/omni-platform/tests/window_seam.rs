//! Tests of the platform-independent half of the window seam: the argument checking that happens
//! before any backend is asked, the diagnostic quality of the error type, and the invariants of
//! the event and handle enums.
//!
//! **These run on every target and need no display.** Everything that needs a real window — and
//! therefore a desktop session — is in `window_live.rs`, gated and `#[ignore]`d.
//!
//! The split exists because VERIFICATION entry 4 is about a test that skipped when its fixture was
//! missing and reported `ok` anyway. A file that could quietly become a no-op on a headless host
//! is the same shape, so the two halves are two files: this one has no fixture to be missing, and
//! the other one **fails** when it is asked to run without one.

use omni_platform::window::{
    MAX_EXTENT, PointerButton, RawWindow, Window, WindowDesc, WindowError, WindowEvent,
};

#[test]
fn a_zero_extent_is_refused_on_every_target_before_any_os_call() {
    // The point of this test is the *uniformity*: the refusal is the same on Windows, where a
    // window could have been created, as on Linux, where it could not. A backend that validated
    // its own arguments would answer "not implemented on linux" to "you asked for zero pixels",
    // which is true and useless.
    for (width, height) in [(0, 480), (640, 0), (0, 0)] {
        let err = Window::new(&WindowDesc::new("omnidroid", width, height)).unwrap_err();
        assert_eq!(
            err,
            WindowError::SizeOutOfRange {
                operation: "create",
                width,
                height,
                max: MAX_EXTENT,
            },
            "{width}x{height} should have been refused as an extent"
        );
        assert!(!err.is_unsupported(), "a bad argument is not a missing backend: {err}");
        // Global Constraint 7: the message names the values that were wrong.
        let text = err.to_string();
        assert!(text.contains(&format!("{width}x{height}")), "{text}");
        assert!(text.contains(&MAX_EXTENT.to_string()), "{text}");
    }
}

#[test]
fn an_extent_past_the_wm_size_limit_is_refused_in_either_axis() {
    // `MAX_EXTENT` is where `WM_SIZE`'s 16-bit halves stop being able to report the size, so one
    // past it must be refused — in *both* axes, because a check written against `width` alone
    // passes every test that only ever makes the width too large.
    for (width, height) in [(MAX_EXTENT + 1, 480), (640, MAX_EXTENT + 1)] {
        let err = Window::new(&WindowDesc::new("omnidroid", width, height)).unwrap_err();
        assert_eq!(
            err,
            WindowError::SizeOutOfRange { operation: "create", width, height, max: MAX_EXTENT },
            "{width}x{height} is past the limit and must be refused"
        );
    }
    assert_eq!(MAX_EXTENT, 65_535, "MAX_EXTENT is the largest value a WM_SIZE half can carry");
    // The other side of the boundary — that `MAX_EXTENT` itself is *not* refused — needs a
    // backend to answer, so it is `window_live.rs`'s
    // `the_largest_accepted_extent_is_not_refused_by_the_seams_own_limit`.
}

#[test]
fn a_title_with_an_interior_nul_is_refused_with_its_offset() {
    let err = Window::new(&WindowDesc::new("omni\0droid", 640, 480)).unwrap_err();
    assert_eq!(err, WindowError::TitleHasInteriorNul { operation: "create", at: 4 });
    // The offset is the whole value of the diagnostic: Win32 would have shown "omni" and said
    // nothing, and "the title was truncated" without an index is a search rather than a fix.
    assert!(err.to_string().contains("index 4"), "{err}");
}

/// A valid description must reach the backend, and on a target without one it must come back as
/// `Unsupported` naming what is missing — not as an argument error, which is the failure the two
/// tests above cover.
///
/// `cfg(target_os)` is allowed here for the reason Global Constraint 4 gives: this is
/// `omni-platform`, and this is a test *of* the per-target split. Linux has a backend (X11), so
/// this is macOS's; `window_linux.rs` carries Linux's side of the same boundary.
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
#[test]
fn a_valid_description_reaches_the_structural_backend_and_is_refused_by_name() {
    let err = Window::new(&WindowDesc::new("omnidroid", 640, 480)).unwrap_err();
    assert!(err.is_unsupported(), "expected the structural refusal, got {err}");
    let text = err.to_string();
    for expected in [std::env::consts::OS, "structural only", "create"] {
        assert!(text.contains(expected), "the refusal must name {expected}: {text}");
    }
    // It names the API to write, not merely that there is none. This is the difference between a
    // refusal that says what the missing work *is* and one that says only that it is missing.
    assert!(
        text.contains("xcb_create_window") || text.contains("NSWindow"),
        "the refusal must name the intended platform API: {text}"
    );
}

#[test]
fn the_unsupported_refusal_is_the_only_one_that_reports_as_unsupported() {
    // `is_unsupported` is what a renderer branches on to tell "this host has no such backend"
    // from "the OS said no", so every other variant must answer false. Asserted over a
    // constructed instance of each variant rather than over a count, which would not notice a new
    // variant answering true.
    let variants = [
        WindowError::Unsupported { operation: "create", intended: "x", platform: "linux" },
        WindowError::LastError { operation: "create", api: "CreateWindowExW", code: 1400 },
        WindowError::SizeOutOfRange { operation: "create", width: 0, height: 0, max: MAX_EXTENT },
        WindowError::TitleHasInteriorNul { operation: "create", at: 0 },
    ];
    let unsupported: Vec<_> = variants.iter().filter(|e| e.is_unsupported()).collect();
    assert_eq!(unsupported.len(), 1, "exactly one variant is a missing backend");
    assert!(unsupported[0].is_unsupported());
    // And every variant names the operation that failed, which is Global Constraint 7's whole
    // requirement of this type.
    for err in &variants {
        assert!(err.to_string().contains("create"), "{err} does not name its operation");
    }
}

#[test]
fn every_pointer_button_is_in_all_and_they_are_distinct() {
    // Membership, not a total (VERIFICATION entry 1): a `len() == 5` assertion passes for a list
    // that names `Primary` twice and omits `Forward`.
    for button in [
        PointerButton::Primary,
        PointerButton::Secondary,
        PointerButton::Middle,
        PointerButton::Back,
        PointerButton::Forward,
    ] {
        assert!(PointerButton::ALL.contains(&button), "{button} is missing from ALL");
    }
    for (i, a) in PointerButton::ALL.iter().enumerate() {
        for b in &PointerButton::ALL[i + 1..] {
            assert_ne!(a, b, "ALL lists {a} twice");
            assert_ne!(a.to_string(), b.to_string(), "{a} and {b} render identically");
        }
    }
}

#[test]
fn a_raw_handle_names_its_windowing_system() {
    // The name exists so that a renderer which does not know a variant can refuse naming it,
    // rather than printing two integers. Asserted against the handle it describes.
    let handle = RawWindow::Win32 { hwnd: 0x1234, hinstance: 0x5678 };
    assert_eq!(handle.system_name(), "win32");
    assert!(format!("{handle:?}").contains("4660"), "the debug form carries the handle: {handle:?}");
}

#[test]
fn a_minimised_window_is_described_by_a_zero_extent_rather_than_by_an_error() {
    // Not a behaviour test — it is a *contract* test, pinned here because the renderer's
    // correctness depends on reading it the same way: `Resized { 0, 0 }` is a legal event and
    // means minimised, and a renderer that treats it as an error will fail on the first minimise.
    let minimised = WindowEvent::Resized { width: 0, height: 0 };
    assert_ne!(minimised, WindowEvent::Resized { width: 1, height: 1 });
    assert!(matches!(minimised, WindowEvent::Resized { width: 0, height: 0 }));
}
