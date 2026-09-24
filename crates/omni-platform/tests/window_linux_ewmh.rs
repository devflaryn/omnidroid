//! **Restoring a minimised window under a manager that restores only through EWMH** -- the way
//! GNOME's Mutter treats an X11 client (Xwayland on the owner's desktop).
//!
//! MEASURED on the port's host (Ubuntu 26.04, GNOME/Mutter, Xwayland `:0`), with `xclock` and
//! `xprop`: after `xdotool windowminimize` the window's `WM_STATE` is `Iconic`; `XMapWindow` (what
//! the backend's restore sent) leaves it `Iconic`; a `_NET_ACTIVE_WINDOW` request makes it
//! `Normal`. xfwm4 restores on either, which is why `window_linux_wm.rs` could not see the gap,
//! and `renderer_live.rs`'s minimise test then waited out its 10 s on the real desktop.
//!
//! A real Mutter cannot run on an Xvfb here, so this file starts its **own** Xvfb and runs a
//! minimal manager with exactly those measured rules in a thread: it owns `WM_S0`, maps windows
//! on `MapRequest` and writes `WM_STATE` `Normal`, iconifies on ICCCM's `WM_CHANGE_STATE`
//! (writes `Iconic`, as Mutter does, without unmapping), **ignores a `MapRequest` from an iconic
//! window**, and restores (`Normal`) on `_NET_ACTIVE_WINDOW`. A model of what was measured, not
//! of Mutter's source; its rules are the three `xprop` readings above.
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-platform --release --test window_linux_ewmh \
//!     -- --ignored --test-threads=1
//! ```

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::os::raw::{c_long, c_ulong};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_platform::window::{Window, WindowDesc, WindowEvent};
use x11_dl::xlib;

const GATE: &str = "OMNI_GFX_WINDOW_TESTS";
/// A display number nobody else on the port's host uses (the workers' are :91-:99).
const DISPLAY: &str = ":87";
const NORMAL: c_long = 1;
const ICONIC: c_long = 3;

/// This test's own X server, killed on drop.
struct Server(Child);

impl Server {
    fn start() -> Server {
        let child = Command::new("Xvfb")
            .args([DISPLAY, "-screen", "0", "1280x800x24", "-noreset"])
            .spawn()
            .expect("Xvfb must be runnable (package xvfb): this test is not skippable");
        let server = Server(child);
        let xl = xlib::Xlib::open().expect("libX11");
        let name = CString::new(DISPLAY).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // SAFETY: a NUL-terminated display name; a connection opened here is closed at once.
            let d = unsafe { (xl.XOpenDisplay)(name.as_ptr()) };
            if !d.is_null() {
                unsafe { (xl.XCloseDisplay)(d) };
                return server;
            }
            assert!(Instant::now() < deadline, "Xvfb {DISPLAY} did not accept a connection in 10 s");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn atom(xl: &xlib::Xlib, display: *mut xlib::Display, name: &str) -> xlib::Atom {
    let name = CString::new(name).unwrap();
    // SAFETY: a live display and a NUL-terminated name.
    unsafe { (xl.XInternAtom)(display, name.as_ptr(), xlib::False) }
}

fn set_wm_state(xl: &xlib::Xlib, display: *mut xlib::Display, window: c_ulong, state: c_long) {
    let wm_state = atom(xl, display, "WM_STATE");
    let data: [c_long; 2] = [state, 0];
    // SAFETY: a live display and window; two 32-bit items read from `data`.
    unsafe {
        (xl.XChangeProperty)(
            display, window, wm_state, wm_state, 32, xlib::PropModeReplace, data.as_ptr().cast(), 2,
        );
    }
}

/// The measured-Mutter manager, on its own connection and thread, until `stop` is set.
fn run_manager(ready: Arc<AtomicBool>, stop: Arc<AtomicBool>, activations: Arc<AtomicBool>) {
    let xl = xlib::Xlib::open().expect("libX11");
    let name = CString::new(DISPLAY).unwrap();
    // SAFETY: a NUL-terminated display name; everything below uses this one connection only.
    let display = unsafe { (xl.XOpenDisplay)(name.as_ptr()) };
    assert!(!display.is_null());
    unsafe {
        let root = (xl.XDefaultRootWindow)(display);
        let owner = (xl.XCreateSimpleWindow)(display, root, 0, 0, 1, 1, 0, 0, 0);
        (xl.XSetSelectionOwner)(display, atom(&xl, display, "WM_S0"), owner, xlib::CurrentTime);
        (xl.XSelectInput)(display, root, xlib::SubstructureRedirectMask | xlib::SubstructureNotifyMask);
        (xl.XSync)(display, xlib::False);
    }
    let change_state = atom(&xl, display, "WM_CHANGE_STATE");
    let active_window = atom(&xl, display, "_NET_ACTIVE_WINDOW");
    let mut iconic: Vec<c_ulong> = Vec::new();
    ready.store(true, Ordering::SeqCst);
    while !stop.load(Ordering::SeqCst) {
        // SAFETY: a live display.
        if unsafe { (xl.XPending)(display) } == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        // SAFETY: a zeroed event is overwritten by XNextEvent; the member read matches its type.
        unsafe {
            let mut event: xlib::XEvent = core::mem::zeroed();
            (xl.XNextEvent)(display, &raw mut event);
            match event.get_type() {
                xlib::MapRequest => {
                    let window = event.map_request.window;
                    if iconic.contains(&window) {
                        continue; // measured: a map does not restore an iconic window
                    }
                    (xl.XMapWindow)(display, window);
                    set_wm_state(&xl, display, window, NORMAL);
                    (xl.XSetInputFocus)(display, window, xlib::RevertToParent, xlib::CurrentTime);
                }
                xlib::ConfigureRequest => {
                    let request = event.configure_request;
                    let mut changes: xlib::XWindowChanges = core::mem::zeroed();
                    changes.x = request.x;
                    changes.y = request.y;
                    changes.width = request.width;
                    changes.height = request.height;
                    (xl.XConfigureWindow)(display, request.window, request.value_mask as u32, &raw mut changes);
                }
                xlib::ClientMessage => {
                    let message = event.client_message;
                    let window = message.window;
                    if message.message_type == change_state && message.data.get_long(0) == ICONIC {
                        set_wm_state(&xl, display, window, ICONIC);
                        if !iconic.contains(&window) {
                            iconic.push(window);
                        }
                    } else if message.message_type == active_window {
                        activations.store(true, Ordering::SeqCst);
                        iconic.retain(|w| *w != window);
                        set_wm_state(&xl, display, window, NORMAL);
                        (xl.XSetInputFocus)(display, window, xlib::RevertToParent, xlib::CurrentTime);
                    }
                }
                _ => {}
            }
            (xl.XFlush)(display);
        }
    }
    unsafe { (xl.XCloseDisplay)(display) };
}

fn poll_until(window: &mut Window, what: &str, want: impl Fn(&WindowEvent) -> bool) -> Vec<WindowEvent> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    loop {
        let batch: Vec<WindowEvent> = window.poll_events().collect();
        let hit = batch.iter().any(&want);
        seen.extend(batch);
        if hit {
            return seen;
        }
        assert!(Instant::now() < deadline, "waited 5s for {what}; what arrived was {seen:?}");
        window.wait(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "starts its own Xvfb and window manager: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_minimised_window_is_restored_by_a_manager_that_restores_only_through_ewmh() {
    assert!(std::env::var(GATE).is_ok_and(|v| v == "1"), "run with --ignored but {GATE} is not 1");
    let _server = Server::start();
    let (ready, stop, activations) =
        (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let manager = {
        let (ready, stop, activations) = (Arc::clone(&ready), Arc::clone(&stop), Arc::clone(&activations));
        std::thread::spawn(move || run_manager(ready, stop, activations))
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the model manager did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: setting the process's DISPLAY before the seam opens its connection; nothing else in
    // this single-test process reads the environment concurrently.
    unsafe { std::env::set_var("DISPLAY", DISPLAY) };

    let mut window = Window::new(&WindowDesc::new("omnidroid: ewmh restore", 400, 300)).unwrap();
    window.show();
    poll_until(&mut window, "the first size", |e| *e == WindowEvent::Resized { width: 400, height: 300 });

    window.set_minimized(true).unwrap();
    poll_until(&mut window, "the minimised size", |e| *e == WindowEvent::Resized { width: 0, height: 0 });

    activations.store(false, Ordering::SeqCst);
    window.set_minimized(false).unwrap();
    let seen = poll_until(&mut window, "the restored size", |e| {
        *e == WindowEvent::Resized { width: 400, height: 300 }
    });
    assert!(activations.load(Ordering::SeqCst), "the restore asked the manager to activate: {seen:?}");
    assert_eq!(window.client_size().unwrap(), (400, 300));

    drop(window);
    stop.store(true, Ordering::SeqCst);
    manager.join().expect("the model manager");
}
