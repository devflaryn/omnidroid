//! Tests that create a **real** window, and therefore need a desktop session.
//!
//! # How this file obeys VERIFICATION entry 4 without breaking a headless CI
//!
//! Entry 4 is the rule that a test which cannot run must **fail**, not skip: a test that
//! early-returns when its fixture is missing reports `ok` and proves nothing, and that is how two
//! of the filesystem seam's six confinement rules came to have never executed on the machine
//! whose green suite was the evidence for them.
//!
//! A window cannot be created without a desktop session, and a build machine may not have one. So
//! the two states are made *visibly different* rather than reconciled:
//!
//! * **Not asked for.** Every test here is `#[ignore]`d with a reason that names the environment
//!   variable. An ordinary `cargo test --workspace --release` reports them as `ignored`, which is
//!   a line of output saying they did not run — not an `ok` claiming they did.
//! * **Asked for.** `cargo test -- --ignored` means someone decided these should run. If
//!   `OMNI_GFX_WINDOW_TESTS=1` is not set, they **panic** naming the variable, because at that
//!   point a silent skip would be exactly entry 4's failure.
//!
//! What this cannot do is notice a host that has a session and is simply not showing it. Nothing
//! can; that is why the gate is an explicit decision by whoever runs the suite rather than a
//! probe.
//!
//! Run them with:
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-platform --release --test window_live -- --ignored
//! ```
//!
//! # Why these are single-threaded-safe as written
//!
//! Each test creates its own window on its own thread, and Win32 delivers a window's messages only
//! to the thread that created it, so two tests running concurrently cannot see each other's
//! events. The one piece of shared state is the process-wide window class, which is registered
//! behind a `OnceLock` — and `two_windows_on_one_thread_do_not_steal_each_others_events` is the
//! test that would notice if that sharing ever leaked into the event path.

use std::time::{Duration, Instant};

use omni_platform::window::{
    MAX_EXTENT, RawWindow, Window, WindowDesc, WindowError, WindowEvent,
};

/// The opt-in. Named for the whole graphics bring-up rather than for this crate, because the
/// renderer's live tests in `omni-gfx` are gated on the same decision by the same person.
const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

/// Fail — loudly, naming the variable — if these were run without the opt-in.
///
/// Reaching this function at all means `--ignored` was passed, i.e. somebody asked for the window
/// tests. Answering that request with a silent success is the defect VERIFICATION entry 4 records.
fn require_gate() {
    let set = std::env::var(GATE).is_ok_and(|v| v == "1");
    assert!(
        set,
        "this test was run with --ignored but {GATE} is not set to 1. It creates a real window \
         and needs a desktop session; it will not pretend to have passed without one. Set \
         {GATE}=1 to run it, or drop --ignored to skip it visibly."
    );
}

/// Poll until `want` is true of some event, or the deadline passes.
///
/// A bounded poll on the thing under test rather than a sleep-and-hope, which VERIFICATION entry 6
/// is the record of: four flakes in this project's sync layer were fixed exactly this way, and a
/// fifth was a `sleep` that was not long enough on a loaded machine. The deadline is generous
/// because it only bounds *failure*; a passing run leaves as soon as the event arrives.
///
/// Returns every event seen along the way, so that a failing assertion can say what *did* arrive
/// instead of only that the wanted thing did not.
fn poll_until(
    window: &mut Window,
    what: &str,
    want: impl Fn(&WindowEvent) -> bool,
) -> Vec<WindowEvent> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    loop {
        let batch: Vec<WindowEvent> = window.poll_events().collect();
        let hit = batch.iter().any(&want);
        seen.extend(batch);
        if hit {
            return seen;
        }
        assert!(
            Instant::now() < deadline,
            "waited 5s for {what} and it never arrived; what did arrive was {seen:?}"
        );
        std::thread::yield_now();
    }
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_new_window_reports_the_client_size_it_was_asked_for() {
    require_gate();
    let window = Window::new(&WindowDesc::new("omnidroid: size", 640, 480)).unwrap();

    // The client area, not the outer window. This is the number a swapchain is built from, and
    // the whole reason `create` measures rather than predicts the frame thickness.
    assert_eq!(
        window.client_size().unwrap(),
        (640, 480),
        "the client area must be exactly what was asked for, whatever the display's scale factor \
         and whatever the frame metrics are"
    );
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn the_initial_size_is_in_the_event_stream_before_the_first_poll() {
    require_gate();
    let mut window = Window::new(&WindowDesc::new("omnidroid: first event", 800, 600)).unwrap();

    // Asserted as membership, not as "the first event" or "one event": the number of `WM_SIZE`
    // messages creation produces is the OS's business and has no reason to be stable, but the
    // *final* size it settles on does (VERIFICATION entry 1).
    let events: Vec<WindowEvent> = window.poll_events().collect();
    assert!(
        events.contains(&WindowEvent::Resized { width: 800, height: 600 }),
        "a runtime that sizes its swapchain from the event stream must learn the initial size \
         without a special first frame; events were {events:?}"
    );
    assert_eq!(window.client_size().unwrap(), (800, 600), "and the OS must agree with the event");
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_drained_event_is_not_handed_out_twice() {
    require_gate();
    let mut window = Window::new(&WindowDesc::new("omnidroid: drain", 640, 480)).unwrap();

    let first: Vec<WindowEvent> = window.poll_events().collect();
    assert!(!first.is_empty(), "creation produces at least the initial resize");
    let second: Vec<WindowEvent> = window.poll_events().collect();
    assert_eq!(second, Vec::new(), "the queue was drained, so nothing is left: {second:?}");
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_resize_is_reported_with_the_new_size_and_the_os_agrees_with_it() {
    require_gate();
    let mut window = Window::new(&WindowDesc::new("omnidroid: resize", 640, 480)).unwrap();
    let _ = window.poll_events().count();

    window.set_client_size(900, 700).unwrap();
    let seen = poll_until(&mut window, "the resize", |e| {
        matches!(e, WindowEvent::Resized { width: 900, height: 700 })
    });

    // Two independent sources have to agree: the event stream and the OS. A backend that
    // fabricated the event from its own arguments would pass the first assertion and fail the
    // second — which is VERIFICATION entry 7's rule in miniature, that agreeing with yourself
    // proves nothing.
    assert!(seen.contains(&WindowEvent::Resized { width: 900, height: 700 }), "{seen:?}");
    assert_eq!(window.client_size().unwrap(), (900, 700));

    // And no stale intermediate size is left in the queue behind it: the coalescing rule says a
    // run of resizes collapses to its newest member, so nothing that is not the current size may
    // survive the poll.
    for event in &seen {
        if let WindowEvent::Resized { width, height } = event {
            assert_eq!(
                (*width, *height),
                (900, 700),
                "a resize naming a size that is no longer current survived the queue: {seen:?}"
            );
        }
    }
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn resizing_to_the_size_it_already_has_produces_no_event() {
    require_gate();
    let mut window = Window::new(&WindowDesc::new("omnidroid: no-op resize", 640, 480)).unwrap();
    let _ = window.poll_events().count();

    window.set_client_size(640, 480).unwrap();
    // A resize event the renderer acts on means destroying and rebuilding a swapchain, which the
    // graphics spike measured as the operation that crashes drivers when it is wrong. Emitting
    // one for a size that did not change would make that happen for nothing, repeatedly.
    let events: Vec<WindowEvent> = window.poll_events().collect();
    let resizes: Vec<&WindowEvent> =
        events.iter().filter(|e| matches!(e, WindowEvent::Resized { .. })).collect();
    assert!(resizes.is_empty(), "a no-op resize must be silent, got {resizes:?}");
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn request_close_is_a_request_and_leaves_the_window_alive() {
    require_gate();
    let mut window = Window::new(&WindowDesc::new("omnidroid: close", 640, 480)).unwrap();
    let _ = window.poll_events().count();

    window.request_close().unwrap();
    poll_until(&mut window, "the close request", |e| *e == WindowEvent::CloseRequested);

    // The load-bearing half. `DefWindowProcW`'s own handling of `WM_CLOSE` is to destroy the
    // window, so a backend that forwarded the message would pass the assertion above and have
    // destroyed the window under a running guest by the time anyone noticed. The window must
    // still answer, and still be the same window.
    assert_eq!(window.client_size().unwrap(), (640, 480), "the window was closed, not asked");
    window.set_client_size(700, 500).unwrap();
    poll_until(&mut window, "a resize after the close request", |e| {
        matches!(e, WindowEvent::Resized { width: 700, height: 500 })
    });
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn two_windows_on_one_thread_do_not_steal_each_others_events() {
    require_gate();
    let mut left = Window::new(&WindowDesc::new("omnidroid: left", 320, 240)).unwrap();
    let mut right = Window::new(&WindowDesc::new("omnidroid: right", 400, 300)).unwrap();
    let _ = left.poll_events().count();
    let _ = right.poll_events().count();

    // D10 runs several guest instances, and a single-threaded host loop driving two windows is
    // the configuration in which an unfiltered `PeekMessageW` misbehaves. It would not lose the
    // event — dispatching still reaches the right window procedure — so the symptom is the
    // strange one: a window that only produces events while a *different* window is polled.
    left.request_close().unwrap();
    poll_until(&mut left, "the left window's close request", |e| *e == WindowEvent::CloseRequested);

    let right_events: Vec<WindowEvent> = right.poll_events().collect();
    assert!(
        !right_events.contains(&WindowEvent::CloseRequested),
        "the right window saw the left window's close request: {right_events:?}"
    );

    // And the sizes stay distinct, which is the other thing a shared queue would blur.
    assert_eq!(left.client_size().unwrap(), (320, 240));
    assert_eq!(right.client_size().unwrap(), (400, 300));
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn the_raw_handle_is_a_live_win32_handle() {
    require_gate();
    let window = Window::new(&WindowDesc::new("omnidroid: handle", 640, 480)).unwrap();

    match window.raw() {
        RawWindow::Win32 { hwnd, hinstance } => {
            // Vulkan's `VkWin32SurfaceCreateInfoKHR` needs both, and a null in either is the
            // failure mode that surfaces as an unexplained `VK_ERROR_INITIALIZATION_FAILED` two
            // layers away.
            assert_ne!(hwnd, 0, "the HWND is null");
            assert_ne!(hinstance, 0, "the HINSTANCE is null");
        }
        other => panic!("this host is Windows and must produce a Win32 handle, got {other:?}"),
    }
    assert_eq!(window.raw().system_name(), "win32");
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn the_largest_accepted_extent_is_not_refused_by_the_seams_own_limit() {
    require_gate();
    // The far side of `window_seam.rs`'s boundary check: `MAX_EXTENT` is inside the seam's limit,
    // so whatever happens next must not be `SizeOutOfRange`. The window is never shown, and the
    // OS clamps it to the virtual screen — which is the point, since the clamped result is what
    // `client_size` reports and what the seam's documentation says to believe.
    let window = Window::new(&WindowDesc::new("omnidroid: huge", MAX_EXTENT, MAX_EXTENT));
    match window {
        Ok(window) => {
            let (width, height) = window.client_size().unwrap();
            assert!(width > 0 && height > 0, "a created window has a non-zero client area");
        }
        Err(err) => assert!(
            !matches!(err, WindowError::SizeOutOfRange { .. }),
            "MAX_EXTENT is inside the seam's own limit and must not be refused by it: {err}"
        ),
    }
}

#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_shown_window_still_answers_and_drops_cleanly() {
    require_gate();
    // The only test that actually puts pixels on the screen. It exists because `show` is the one
    // operation with no return value to check: the failure it would have is a window that never
    // appears, and the closest a test can get to noticing that is that the window keeps working
    // and tears down without the OS complaining.
    let mut window = Window::new(&WindowDesc::new("omnidroid: shown", 480, 360)).unwrap();
    window.show();
    window.show(); // idempotent

    poll_until(&mut window, "focus after being shown", |e| {
        matches!(e, WindowEvent::FocusChanged { focused: true })
    });
    assert_eq!(window.client_size().unwrap(), (480, 360));
    drop(window);
}
