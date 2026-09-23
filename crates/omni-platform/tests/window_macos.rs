//! **The macOS window backend, live**: what `tests/window_live.rs` (the seam's own contract, which
//! this backend also runs) does not reach -- minimise, the DPI against the host's own scale, the
//! events a real `NSEvent` produces through the real handlers, pointer capture, the AppKit thread,
//! and what the main-thread hand-over does to an ordinary test binary.
//!
//! Gated as `window_live.rs` is (VERIFICATION entry 4): `#[ignore]`d, and under `--ignored` without
//! `OMNI_GFX_WINDOW_TESTS=1` every test **fails** naming the variable.
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-platform --release --test window_macos -- --ignored --test-threads=1
//! ```
//!
//! # Where the input comes from, and what that can and cannot prove (VERIFICATION entry 20)
//!
//! No test can move this machine's mouse or press its keys, so the events are **made by the host's
//! own constructors** -- `+[NSEvent keyEventWithType:…]`, `+[NSEvent mouseEventWithType:…]`, and
//! `CGEventCreate*` turned into an `NSEvent` by `+[NSEvent eventWithCGEvent:]` -- and delivered
//! either through `-[NSWindow sendEvent:]` (the path AppKit's own dispatch takes: first responder
//! for keys, hit-testing for mouse buttons) or, where hit-testing a synthetic screen position is not
//! meaningful, straight to the view's handler (`scrollWheel:`, `otherMouseDown:`, `mouseMoved:`).
//! That proves the handlers read AppKit's events correctly and the seam gets the right events out.
//! It does **not** prove what a physical device produces: the sign AppKit gives a real wheel
//! under "natural" scrolling, and whether a real mouse's captured deltas are accelerated, are
//! stated from documentation in the backend and are not measured here.

#![cfg(target_os = "macos")]

use core::ffi::c_void;
use std::process::Command;
use std::time::{Duration, Instant};

use objc2::encode::{Encode, Encoding};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::{NSPoint, NSString};
use omni_platform::window::{PointerButton, RawWindow, Window, WindowDesc, WindowError, WindowEvent};

const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

fn require_gate() {
    assert!(
        std::env::var(GATE).is_ok_and(|v| v == "1"),
        "this test was run with --ignored but {GATE} is not set to 1. It creates real windows and \
         needs a desktop session; set {GATE}=1 to run it, or drop --ignored to skip it visibly."
    );
}

// ------------------------------------------------------------------------ host access

#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}

#[allow(non_upper_case_globals)]
unsafe extern "C" {
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_sync_f(queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

/// A `CGEventRef`, encoded as AppKit declares it so that message sends carrying one type-check.
#[repr(transparent)]
#[derive(Clone, Copy)]
struct CgEvent(*mut c_void);
unsafe impl Encode for CgEvent {
    const ENCODING: Encoding = Encoding::Pointer(&Encoding::Struct("__CGEvent", &[]));
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreateScrollWheelEvent2(source: *const c_void, units: u32, count: u32, wheel1: i32, wheel2: i32, wheel3: i32) -> CgEvent;
    fn CGEventCreateMouseEvent(source: *const c_void, kind: u32, at: NSPoint, button: u32) -> CgEvent;
    fn CGEventCreateKeyboardEvent(source: *const c_void, key: u16, down: bool) -> CgEvent;
    fn CGEventSetType(event: CgEvent, kind: u32);
    fn CGEventSetFlags(event: CgEvent, flags: u64);
    fn CGEventSetIntegerValueField(event: CgEvent, field: u32, value: i64);
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *mut c_void);
}

/// Run `work` on the AppKit thread (the process's main thread), as the backend does.
fn on_main<R, W: FnOnce() -> R>(work: W) -> R {
    struct Job<F, R>(Option<F>, Option<R>);
    extern "C" fn run<F: FnOnce() -> R, R>(context: *mut c_void) {
        let job = unsafe { &mut *context.cast::<Job<F, R>>() };
        job.1 = Some((job.0.take().unwrap())());
    }
    let mut job = Job(Some(work), None);
    unsafe { dispatch_sync_f(&raw const _dispatch_main_q, (&raw mut job).cast(), run::<W, R>) };
    job.1.expect("the main queue ran the work")
}

/// The `(NSWindow *, NSView *)` of a window.
fn handles(window: &Window) -> (usize, usize) {
    match window.raw() {
        RawWindow::AppKit { ns_window, ns_view, ca_metal_layer } => {
            assert!(ns_window != 0 && ns_view != 0 && ca_metal_layer != 0, "{:?}", window.raw());
            (ns_window as usize, ns_view as usize)
        }
        other => panic!("a macOS window must have an AppKit handle, got {other:?}"),
    }
}

fn object(address: usize) -> &'static AnyObject {
    unsafe { &*(address as *const AnyObject) }
}

/// `+[NSEvent eventWithCGEvent:]`, then release the CG event.
fn ns_event(cg: CgEvent) -> Retained<AnyObject> {
    let event: Option<Retained<AnyObject>> = unsafe { msg_send![class!(NSEvent), eventWithCGEvent: cg] };
    unsafe { CFRelease(cg.0) };
    event.expect("eventWithCGEvent: made an event")
}

/// `+[NSEvent keyEventWithType:…]` for window `window`, sent through `-[NSWindow sendEvent:]`.
fn send_key(window: usize, down: bool, key_code: u16, characters: &str, flags: u64) {
    on_main(|| {
        let number: isize = unsafe { msg_send![object(window), windowNumber] };
        let text = NSString::from_str(characters);
        let kind: usize = if down { 10 } else { 11 }; // NSEventTypeKeyDown / KeyUp
        let event: Option<Retained<AnyObject>> = unsafe {
            msg_send![class!(NSEvent), keyEventWithType: kind, location: NSPoint::new(0.0, 0.0),
                modifierFlags: flags as usize, timestamp: 0.0_f64, windowNumber: number,
                context: core::ptr::null::<AnyObject>(), characters: &*text,
                charactersIgnoringModifiers: &*text, isARepeat: false, keyCode: key_code]
        };
        let event = event.expect("a key event");
        let _: () = unsafe { msg_send![object(window), sendEvent: &*event] };
    });
}

/// `+[NSEvent mouseEventWithType:…]` at `at` in window points, through `-[NSWindow sendEvent:]`.
fn send_mouse(window: usize, kind: usize, at: NSPoint) {
    on_main(|| {
        let number: isize = unsafe { msg_send![object(window), windowNumber] };
        let event: Option<Retained<AnyObject>> = unsafe {
            msg_send![class!(NSEvent), mouseEventWithType: kind, location: at, modifierFlags: 0_usize,
                timestamp: 0.0_f64, windowNumber: number, context: core::ptr::null::<AnyObject>(),
                eventNumber: 0_isize, clickCount: 1_isize, pressure: 1.0_f32]
        };
        let event = event.expect("a mouse event");
        let _: () = unsafe { msg_send![object(window), sendEvent: &*event] };
    });
}

fn scale_of(window: usize) -> f64 {
    on_main(|| unsafe { msg_send![object(window), backingScaleFactor] })
}

fn view_height_points(view: usize) -> f64 {
    let rect: objc2_foundation::NSRect = on_main(|| unsafe { msg_send![object(view), bounds] });
    rect.size.height
}

// ---------------------------------------------------------------------------- helpers

fn open(title: &str, width: u32, height: u32) -> Window {
    let mut window = Window::new(&WindowDesc::new(title, width, height))
        .unwrap_or_else(|err| panic!("could not create {title}: {err}"));
    let _ = window.poll_events().count();
    window
}

fn drain(window: &mut Window) -> Vec<WindowEvent> {
    window.poll_events().collect()
}

fn poll_until(window: &mut Window, what: &str, want: impl Fn(&WindowEvent) -> bool) -> Vec<WindowEvent> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    loop {
        let batch = drain(window);
        let hit = batch.iter().any(&want);
        seen.extend(batch);
        if hit {
            return seen;
        }
        assert!(Instant::now() < deadline, "waited 5s for {what}; saw {seen:?}");
        window.wait(Duration::from_millis(50));
    }
}

// ------------------------------------------------------------------------------ tests

/// The premise: the test thread is **not** the main thread (libtest's), and a window is still
/// created -- on the AppKit thread, which is.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_window_is_made_from_a_thread_that_is_not_the_main_thread() {
    require_gate();
    assert_eq!(unsafe { libc::pthread_main_np() }, 0, "libtest ran this test on the main thread");
    let window = open("omnidroid: off-main", 320, 240);
    let (ns_window, _) = handles(&window);
    let on_main_thread: i32 = on_main(|| unsafe { libc::pthread_main_np() });
    assert_eq!(on_main_thread, 1, "the main queue runs on the main thread");
    let title: Retained<NSString> = on_main(|| unsafe { msg_send![object(ns_window), title] });
    assert_eq!(title.to_string(), "omnidroid: off-main");
    assert_eq!(window.raw().system_name(), "appkit");
}

/// `dpi` is 96 × the backing scale AppKit reports for the window, and the client size is the
/// view's bounds in points times that scale: two independent readings that must agree.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn dpi_and_size_follow_the_backing_scale() {
    require_gate();
    let window = open("omnidroid: scale", 640, 480);
    let (ns_window, ns_view) = handles(&window);
    let scale = scale_of(ns_window);
    println!("backingScaleFactor {scale}, dpi {}", window.dpi().unwrap());
    assert_eq!(window.dpi().unwrap(), (96.0 * scale).round() as u32);
    let points = view_height_points(ns_view);
    assert_eq!(window.client_size().unwrap().1, (points * scale).round() as u32);
    assert_eq!(window.client_size().unwrap(), (640, 480));
}

/// Odd pixel sizes on a 2x display are half points; recorded, and the seam's rule applies: what
/// `client_size` says afterwards is what to believe, and the event agrees with it.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn an_odd_pixel_size_is_reported_as_the_host_made_it() {
    require_gate();
    let mut window = open("omnidroid: odd", 640, 480);
    window.set_client_size(641, 481).unwrap();
    let got = window.client_size().unwrap();
    println!("asked for 641x481, the host made {got:?}");
    let seen = poll_until(&mut window, "the resize", |e| matches!(e, WindowEvent::Resized { .. }));
    assert!(seen.contains(&WindowEvent::Resized { width: got.0, height: got.1 }), "{seen:?}");
}

/// Minimise: a `Resized { 0, 0 }` and a zero client size; restore: the size comes back, in both.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn minimise_reports_zero_and_restore_reports_the_size_again() {
    require_gate();
    let mut window = open("omnidroid: minimise", 400, 300);
    window.show();
    window.set_minimized(true).unwrap();
    poll_until(&mut window, "the minimise", |e| *e == WindowEvent::Resized { width: 0, height: 0 });
    assert_eq!(window.client_size().unwrap(), (0, 0));
    window.set_minimized(false).unwrap();
    poll_until(&mut window, "the restore", |e| *e == WindowEvent::Resized { width: 400, height: 300 });
    assert_eq!(window.client_size().unwrap(), (400, 300));
}

/// `wait` wakes for an event queued from the AppKit thread, and times out without one.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn wait_wakes_for_an_event_and_times_out_without_one() {
    require_gate();
    let mut window = open("omnidroid: wait", 320, 240);
    let started = Instant::now();
    assert!(!window.wait(Duration::from_millis(150)), "nothing was queued");
    let idled = started.elapsed();
    assert!(idled >= Duration::from_millis(140), "the wait returned after {idled:?}");
    window.request_close().unwrap();
    let started = Instant::now();
    assert!(window.wait(Duration::from_secs(5)));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(drain(&mut window), vec![WindowEvent::CloseRequested]);
}

/// Keys through `-[NSWindow sendEvent:]`: `keycode` is the kVK, `scancode` the set-1 code of the
/// same physical key, typed text follows its key, and Return (a control character) types nothing.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn keys_carry_the_virtual_key_and_the_physical_set1_code_and_type_text() {
    require_gate();
    let mut window = open("omnidroid: keys", 320, 240);
    let (ns_window, _) = handles(&window);
    send_key(ns_window, true, 0x0C, "q", 0); // kVK_ANSI_Q
    send_key(ns_window, false, 0x0C, "q", 0);
    send_key(ns_window, true, 0x24, "\r", 0); // kVK_Return
    send_key(ns_window, true, 0x7E, "\u{F700}", 0); // kVK_UpArrow, NSUpArrowFunctionKey
    send_key(ns_window, true, 0x00, "é", 0); // what a layout could make of kVK_ANSI_A
    let events = drain(&mut window);
    assert_eq!(
        events,
        vec![
            WindowEvent::KeyDown { keycode: 0x0C, scancode: 0x10, repeat: false },
            WindowEvent::Text { text: "q".to_owned() },
            WindowEvent::KeyUp { keycode: 0x0C, scancode: 0x10 },
            WindowEvent::KeyDown { keycode: 0x24, scancode: 0x1C, repeat: false },
            WindowEvent::KeyDown { keycode: 0x7E, scancode: 0xE048, repeat: false },
            WindowEvent::KeyDown { keycode: 0x00, scancode: 0x1E, repeat: false },
            WindowEvent::Text { text: "é".to_owned() },
        ]
    );
    // Command held: a shortcut, not typing.
    send_key(ns_window, true, 0x00, "a", 1 << 20);
    assert_eq!(drain(&mut window), vec![WindowEvent::KeyDown { keycode: 0x00, scancode: 0x1E, repeat: false }]);
}

/// Modifiers arrive as `flagsChanged:`, and the device-dependent bit decides down or up per side.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn modifiers_are_reported_down_and_up_per_physical_key() {
    require_gate();
    let mut window = open("omnidroid: modifiers", 320, 240);
    let (ns_window, _) = handles(&window);
    let flags_changed = |key: u16, flags: u64| {
        on_main(|| {
            let cg = unsafe { CGEventCreateKeyboardEvent(core::ptr::null(), key, true) };
            unsafe {
                CGEventSetType(cg, 12); // kCGEventFlagsChanged
                CGEventSetFlags(cg, flags);
            }
            let event = ns_event(cg);
            let _: () = unsafe { msg_send![object(ns_window), sendEvent: &*event] };
        });
    };
    const SHIFT: u64 = 0x0002_0000; // kCGEventFlagMaskShift
    flags_changed(0x38, SHIFT | 0x2); // left Shift down: NX_DEVICELSHIFTKEYMASK
    flags_changed(0x3C, SHIFT | 0x2 | 0x4); // right Shift down too
    flags_changed(0x38, SHIFT | 0x4); // left up, right still down
    flags_changed(0x3C, 0); // right up
    flags_changed(0x39, 0x0001_0000); // Caps Lock, a toggle
    assert_eq!(
        drain(&mut window),
        vec![
            WindowEvent::KeyDown { keycode: 0x38, scancode: 0x2A, repeat: false },
            WindowEvent::KeyDown { keycode: 0x3C, scancode: 0x36, repeat: false },
            WindowEvent::KeyUp { keycode: 0x38, scancode: 0x2A },
            WindowEvent::KeyUp { keycode: 0x3C, scancode: 0x36 },
            WindowEvent::KeyDown { keycode: 0x39, scancode: 0x3A, repeat: false },
            WindowEvent::KeyUp { keycode: 0x39, scancode: 0x3A },
        ]
    );
}

/// Buttons through hit-testing, at a known point: client pixels, origin top-left.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn buttons_are_reported_at_their_pixel_from_the_top_left() {
    require_gate();
    let mut window = open("omnidroid: buttons", 400, 300);
    // Hit-testing needs a window on screen: `sendEvent:` delivers no mouse event to one that is not.
    window.show();
    poll_until(&mut window, "the focus", |e| *e == WindowEvent::FocusChanged { focused: true });
    let (ns_window, ns_view) = handles(&window);
    let scale = scale_of(ns_window);
    let height = view_height_points(ns_view);
    let at = NSPoint::new(10.25, 20.5); // window points, from the bottom-left
    let (x, y) = ((10.25 * scale).floor() as i32, ((height - 20.5) * scale).floor() as i32);
    send_mouse(ns_window, 1, at); // NSEventTypeLeftMouseDown
    send_mouse(ns_window, 2, at); // LeftMouseUp
    send_mouse(ns_window, 3, at); // RightMouseDown
    send_mouse(ns_window, 4, at); // RightMouseUp
    // Only the buttons: the window is on screen, and a real pointer passing over it would add
    // `PointerMoved`s that are nobody's business here.
    let buttons: Vec<WindowEvent> = drain(&mut window)
        .into_iter()
        .filter(|e| matches!(e, WindowEvent::PointerDown { .. } | WindowEvent::PointerUp { .. }))
        .collect();
    assert_eq!(
        buttons,
        vec![
            WindowEvent::PointerDown { button: PointerButton::Primary, x, y },
            WindowEvent::PointerUp { button: PointerButton::Primary, x, y },
            WindowEvent::PointerDown { button: PointerButton::Secondary, x, y },
            WindowEvent::PointerUp { button: PointerButton::Secondary, x, y },
        ]
    );
    // The other buttons: 2 is the middle, 3 and 4 the side buttons, 5 has no seam button.
    for (number, button) in [(2u32, Some(PointerButton::Middle)), (3, Some(PointerButton::Back)), (4, Some(PointerButton::Forward)), (5, None)] {
        on_main(|| {
            let cg = unsafe { CGEventCreateMouseEvent(core::ptr::null(), 25, NSPoint::new(0.0, 0.0), number) };
            let event = ns_event(cg);
            let _: () = unsafe { msg_send![object(ns_view), otherMouseDown: &*event] };
        });
        let got: Vec<Option<PointerButton>> = drain(&mut window)
            .into_iter()
            .filter_map(|e| match e {
                WindowEvent::PointerDown { button, .. } => Some(Some(button)),
                _ => None,
            })
            .collect();
        assert_eq!(got, button.map(Some).into_iter().collect::<Vec<_>>(), "button number {number}");
    }
}

/// A line of wheel is 120, away from the user is positive `dy`, and AppKit's positive horizontal
/// (leftward) comes out as negative `dx`. The events are CoreGraphics' own line-unit scroll events,
/// which is what a notched wheel produces; `isDirectionInvertedFromDevice` is false for them.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn wheel_lines_are_120_each_with_physical_signs() {
    require_gate();
    let mut window = open("omnidroid: wheel", 400, 300);
    let (_, ns_view) = handles(&window);
    let scroll = |vertical: i32, horizontal: i32| {
        on_main(|| {
            let cg = unsafe { CGEventCreateScrollWheelEvent2(core::ptr::null(), 1, 2, vertical, horizontal, 0) };
            let event = ns_event(cg);
            let dy: f64 = unsafe { msg_send![&*event, deltaY] };
            let dx: f64 = unsafe { msg_send![&*event, deltaX] };
            let inverted: bool = unsafe { msg_send![&*event, isDirectionInvertedFromDevice] };
            println!("wheel1 {vertical}, wheel2 {horizontal}: deltaY {dy}, deltaX {dx}, inverted {inverted}");
            let _: () = unsafe { msg_send![object(ns_view), scrollWheel: &*event] };
        });
    };
    scroll(1, 0);
    scroll(-2, 0);
    scroll(0, 1);
    let wheels: Vec<(i32, i32)> = drain(&mut window)
        .into_iter()
        .filter_map(|e| match e {
            WindowEvent::Wheel { dx, dy, .. } => Some((dx, dy)),
            _ => None,
        })
        .collect();
    assert_eq!(wheels, vec![(0, 120), (0, -240), (-120, 0)]);
}

/// Capture: only for the key window; while held, motion arrives as `PointerMotion` from the
/// event's deltas and not as `PointerMoved`; the focus leaving ends it and says so.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_capture_is_granted_to_the_focused_window_and_ends_with_the_focus() {
    require_gate();
    let mut window = open("omnidroid: capture", 400, 300);
    assert_eq!(window.set_pointer_capture(true), Ok(false), "a window never shown has no focus");
    assert!(!window.has_pointer_capture());

    window.show();
    poll_until(&mut window, "the focus", |e| *e == WindowEvent::FocusChanged { focused: true });
    assert_eq!(window.set_pointer_capture(true), Ok(true));
    assert!(window.has_pointer_capture());
    assert_eq!(window.set_pointer_capture(true), Ok(true), "asking again changes nothing");

    let (_, ns_view) = handles(&window);
    on_main(|| {
        let cg = unsafe { CGEventCreateMouseEvent(core::ptr::null(), 5, NSPoint::new(0.0, 0.0), 0) };
        unsafe {
            CGEventSetIntegerValueField(cg, 4, 7); // kCGMouseEventDeltaX
            CGEventSetIntegerValueField(cg, 5, -3); // kCGMouseEventDeltaY
        }
        let event = ns_event(cg);
        let _: () = unsafe { msg_send![object(ns_view), mouseMoved: &*event] };
    });
    assert_eq!(drain(&mut window), vec![WindowEvent::PointerMotion { dx: 7, dy: -3 }]);

    // Another window takes the focus.
    let other = open("omnidroid: capture thief", 200, 150);
    other.show();
    let seen = poll_until(&mut window, "the capture ending", |e| *e == WindowEvent::PointerCaptureLost);
    let lost = seen.iter().position(|e| *e == WindowEvent::PointerCaptureLost).unwrap();
    assert_eq!(seen.get(lost + 1), Some(&WindowEvent::FocusChanged { focused: false }), "{seen:?}");
    assert!(!window.has_pointer_capture());
    drop(other);
}

// ---------------------------------------------------------------- the main-thread hand-over

/// The child side of the tests below: does what `OMNI_MACOS_CHILD` says. Never run on its own.
#[test]
#[ignore = "run only as a child by the hand-over tests below"]
fn child() {
    match std::env::var("OMNI_MACOS_CHILD").as_deref() {
        Ok("panic") => panic!("the child panicked on purpose"),
        Ok("exit") => std::process::exit(7),
        Ok("window") => match Window::new(&WindowDesc::new("omnidroid: child", 64, 64)) {
            Ok(_) => println!("CHILD-WINDOW: created"),
            Err(WindowError::MainThreadUnavailable { why, .. }) => println!("CHILD-WINDOW: refused: {why}"),
            Err(other) => println!("CHILD-WINDOW: other error: {other}"),
        },
        _ => {}
    }
}

fn run_child(binary: &std::path::Path, mode: &str) -> std::process::Output {
    Command::new(binary)
        .args(["--ignored", "--exact", "child", "--nocapture", "--test-threads=1"])
        .env("OMNI_MACOS_CHILD", mode)
        .output()
        .expect("the test binary runs as a child")
}

/// With `main` on a pthread, a test binary still ends the way it would have: a passing run 0, a
/// panicking test 101 with its message printed, `std::process::exit(7)` 7.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn the_hand_over_keeps_exit_codes_and_panic_messages() {
    require_gate();
    let me = std::env::current_exe().unwrap();
    let ok = run_child(&me, "ok");
    assert_eq!(ok.status.code(), Some(0), "{ok:?}");
    let panicked = run_child(&me, "panic");
    assert_eq!(panicked.status.code(), Some(101), "{panicked:?}");
    let text = String::from_utf8_lossy(&panicked.stdout).into_owned() + &String::from_utf8_lossy(&panicked.stderr);
    assert!(text.contains("the child panicked on purpose"), "{text}");
    let exited = run_child(&me, "exit");
    assert_eq!(exited.status.code(), Some(7), "{exited:?}");
    let window = run_child(&me, "window");
    assert!(String::from_utf8_lossy(&window.stdout).contains("CHILD-WINDOW: created"), "{window:?}");
}

/// Inside an application bundle the constructor declines, and the seam **refuses** with the reason
/// rather than hanging or throwing: the same binary, copied to `X.app/Contents/MacOS/`.
#[test]
#[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn inside_an_app_bundle_the_window_is_refused_by_name() {
    require_gate();
    let me = std::env::current_exe().unwrap();
    let bundle = std::env::temp_dir().join(format!("omni-window-{}.app/Contents/MacOS", std::process::id()));
    std::fs::create_dir_all(&bundle).unwrap();
    let copy = bundle.join("child");
    std::fs::copy(&me, &copy).unwrap();
    let output = run_child(&copy, "window");
    let _ = std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(stdout.contains("CHILD-WINDOW: refused") && stdout.contains("application bundle"), "{stdout}");
}

// ------------------------------------------------------------------- a second initializer
//
// MEASURED with `otool -s __DATA_CONST __mod_init_func`: in this binary the linker puts this
// test crate's initializer **after** the constructor, so this is the case where the constructor
// has to run it itself.

/// Set by [`late_initializer`], a second entry in this binary's `__mod_init_func`.
static LATE_RAN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Whether it ran on the main thread, as dyld runs initializers.
static LATE_ON_MAIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn late_initializer(_argc: i32, _argv: *const *const i8, _envp: *const *const i8, _apple: *const *const i8, _vars: *const c_void) {
    LATE_RAN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    LATE_ON_MAIN.store(unsafe { libc::pthread_main_np() } == 1, std::sync::atomic::Ordering::SeqCst);
}

#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static LATE: extern "C" fn(i32, *const *const i8, *const *const i8, *const *const i8, *const c_void) = late_initializer;

/// **Every other initializer still runs exactly once, on the main thread, before `main`** -- the
/// hand-over's promise, since the constructor never returns to dyld. Where this binary's own
/// initializer lands relative to the constructor is the linker's choice; the test says which.
#[test]
fn another_initializer_runs_once_on_the_main_thread_before_main() {
    assert_eq!(LATE_RAN.load(std::sync::atomic::Ordering::SeqCst), 1, "the other initializer ran a different number of times");
    assert!(LATE_ON_MAIN.load(std::sync::atomic::Ordering::SeqCst), "it ran off the main thread");
}
