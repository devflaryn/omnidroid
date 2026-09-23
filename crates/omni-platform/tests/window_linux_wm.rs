//! **The X11 backend under a real window manager**: the half of `window_linux.rs` that only a
//! manager can exercise, because ICCCM gives it the job -- iconifying a window, closing it from
//! the frame, deciding who has the focus.
//!
//! Run against an Xvfb with a manager on it (the port's is `:93` with xfwm4):
//!
//! ```text
//! Xvfb :93 -screen 0 1920x1080x24 &  DISPLAY=:93 xfwm4 --compositor=off &
//! OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:93 cargo test -p omni-platform --release \
//!     --test window_linux_wm -- --ignored --test-threads=1
//! ```
//!
//! Each test **fails** when no manager owns `WM_S0`, rather than passing without one
//! (VERIFICATION entry 4): the point of this file is the manager.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::os::raw::{c_long, c_ulong};
use std::time::{Duration, Instant};

use omni_platform::window::{RawWindow, Window, WindowDesc, WindowEvent};
use x11_dl::xlib;

const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

/// This file's own connection, for what a manager is asked through the root window.
struct Peer {
    xl: xlib::Xlib,
    display: *mut xlib::Display,
}

impl Peer {
    /// Open the connection and fail unless the gate is set and a window manager runs.
    fn open_with_a_manager() -> Peer {
        assert!(
            std::env::var(GATE).is_ok_and(|v| v == "1"),
            "run with --ignored but {GATE} is not 1; these create real X windows"
        );
        let xl = xlib::Xlib::open().expect("libX11");
        // SAFETY: a null name is `$DISPLAY`.
        let display = unsafe { (xl.XOpenDisplay)(std::ptr::null()) };
        assert!(!display.is_null(), "could not connect to $DISPLAY");
        let peer = Peer { xl, display };
        // SAFETY: a live display and an interned atom.
        let owner = unsafe { (peer.xl.XGetSelectionOwner)(peer.display, peer.atom("WM_S0")) };
        assert_ne!(
            owner, 0,
            "no window manager owns WM_S0 on $DISPLAY; this file is the manager's half -- run it \
             on a display with one (DISPLAY=:93 xfwm4 --compositor=off)"
        );
        peer
    }

    /// `window`'s `WM_STATE` state as the server holds it now, read on this connection.
    fn wm_state(&self, window: c_ulong) -> Option<c_long> {
        let atom = self.atom("WM_STATE");
        let (mut kind, mut format, mut count, mut after) = (0, 0, 0, 0);
        let mut data: *mut u8 = std::ptr::null_mut();
        // SAFETY: a live display and window id and writable out-parameters; `data` is freed.
        unsafe {
            (self.xl.XGetWindowProperty)(
                self.display, window, atom, 0, 2, xlib::False, atom, &raw mut kind, &raw mut format,
                &raw mut count, &raw mut after, &raw mut data,
            );
            let state = (!data.is_null() && format == 32 && count >= 1).then(|| *data.cast::<c_long>());
            if !data.is_null() {
                (self.xl.XFree)(data.cast());
            }
            state
        }
    }

    fn atom(&self, name: &str) -> xlib::Atom {
        let name = CString::new(name).unwrap();
        // SAFETY: a live display and a NUL-terminated name.
        unsafe { (self.xl.XInternAtom)(self.display, name.as_ptr(), xlib::False) }
    }

    /// Ask the manager to close `window` as its own close button does: EWMH `_NET_CLOSE_WINDOW`
    /// to the root. What the manager does then -- a `WM_DELETE_WINDOW` message, or `XKillClient`
    /// for a window that does not advertise it -- is the manager's decision, not this test's.
    fn ask_manager_to_close(&self, window: c_ulong) {
        // SAFETY: a zeroed client message is valid; every field read is set; a live display.
        unsafe {
            let mut message: xlib::XClientMessageEvent = std::mem::zeroed();
            message.type_ = xlib::ClientMessage;
            message.window = window;
            message.message_type = self.atom("_NET_CLOSE_WINDOW");
            message.format = 32;
            message.data.set_long(0, xlib::CurrentTime as c_long);
            message.data.set_long(1, 2); // source indication: a pager, i.e. the user
            let mut event = xlib::XEvent { client_message: message };
            let root = (self.xl.XDefaultRootWindow)(self.display);
            (self.xl.XSendEvent)(
                self.display,
                root,
                xlib::False,
                xlib::SubstructureRedirectMask | xlib::SubstructureNotifyMask,
                &raw mut event,
            );
            (self.xl.XSync)(self.display, xlib::False);
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        // SAFETY: the connection this struct opened, closed once.
        unsafe { (self.xl.XCloseDisplay)(self.display) };
    }
}

fn poll_until(window: &mut Window, what: &str, want: impl Fn(&WindowEvent) -> bool) -> Vec<WindowEvent> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    loop {
        let batch: Vec<WindowEvent> = window.poll_events().collect();
        let hit = batch.iter().any(&want);
        seen.extend(batch);
        if hit {
            return seen;
        }
        assert!(Instant::now() < deadline, "waited 10s for {what}; what arrived was {seen:?}");
        window.wait(Duration::from_millis(50));
    }
}

fn xid(window: &Window) -> c_ulong {
    match window.raw() {
        RawWindow::Xlib { window, .. } => window as c_ulong,
        other => panic!("{other:?}"),
    }
}

/// **Shown under a manager, the window is activated** -- through `_NET_ACTIVE_WINDOW`, the
/// manager's call to make -- and the focus arrives as an event.
#[test]
#[ignore = "needs an X server with a window manager: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:93"]
fn a_shown_window_is_activated_by_the_manager() {
    let _peer = Peer::open_with_a_manager();
    let mut window = Window::new(&WindowDesc::new("omnidroid: wm focus", 480, 360)).unwrap();
    window.show();
    poll_until(&mut window, "the focus", |e| *e == WindowEvent::FocusChanged { focused: true });
    assert_eq!(window.client_size().unwrap(), (480, 360), "the frame is the manager's, not ours");
}

/// **The manager's close is a request, and the connection survives it.** A manager closes a
/// window that does not list `WM_DELETE_WINDOW` by killing its client -- which would end this
/// test process through Xlib's I/O error handler -- so arriving at the assertions after the close
/// at all is half of what this proves.
#[test]
#[ignore = "needs an X server with a window manager: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:93"]
fn the_managers_close_is_a_close_request_and_the_client_survives() {
    let peer = Peer::open_with_a_manager();
    let mut window = Window::new(&WindowDesc::new("omnidroid: wm close", 400, 300)).unwrap();
    window.show();
    poll_until(&mut window, "the focus", |e| *e == WindowEvent::FocusChanged { focused: true });
    peer.ask_manager_to_close(xid(&window));
    poll_until(&mut window, "the close request", |e| *e == WindowEvent::CloseRequested);
    // Still alive, still answering, still resizable.
    assert_eq!(window.client_size().unwrap(), (400, 300));
    window.set_client_size(420, 310).unwrap();
    poll_until(&mut window, "a resize after the close", |e| {
        *e == WindowEvent::Resized { width: 420, height: 310 }
    });
}

/// **Minimised is 0x0, restored is the size again** -- from the manager's `WM_STATE`, in the event
/// stream and from `client_size`, which asks the server.
#[test]
#[ignore = "needs an X server with a window manager: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:93"]
fn minimising_is_a_zero_extent_and_restoring_gives_the_size_back() {
    let _peer = Peer::open_with_a_manager();
    let mut window = Window::new(&WindowDesc::new("omnidroid: wm minimise", 500, 400)).unwrap();
    window.show();
    poll_until(&mut window, "the focus", |e| *e == WindowEvent::FocusChanged { focused: true });

    window.set_minimized(true).unwrap();
    poll_until(&mut window, "the minimised size", |e| *e == WindowEvent::Resized { width: 0, height: 0 });
    assert_eq!(window.client_size().unwrap(), (0, 0), "the server's WM_STATE says iconic");

    window.set_minimized(false).unwrap();
    poll_until(&mut window, "the restored size", |e| *e == WindowEvent::Resized { width: 500, height: 400 });
    assert_eq!(window.client_size().unwrap(), (500, 400));
}

/// **A resize through the manager** is reported with the size the manager gave, and the server
/// agrees -- the `window_live.rs` resize, under a manager that intercepts it.
#[test]
#[ignore = "needs an X server with a window manager: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:93"]
fn a_resize_under_the_manager_is_reported_and_the_server_agrees() {
    let _peer = Peer::open_with_a_manager();
    let mut window = Window::new(&WindowDesc::new("omnidroid: wm resize", 640, 480)).unwrap();
    window.show();
    poll_until(&mut window, "the focus", |e| *e == WindowEvent::FocusChanged { focused: true });
    window.set_client_size(900, 700).unwrap();
    poll_until(&mut window, "the resize", |e| *e == WindowEvent::Resized { width: 900, height: 700 });
    assert_eq!(window.client_size().unwrap(), (900, 700));
}

/// **`client_size` says 0x0 at the moment the event stream does, not before.** The server's
/// `WM_STATE` is iconic before this process has read the `PropertyNotify` that says so; a
/// `client_size` that read the property live would answer 0x0 while the events -- which a
/// renderer is fed from -- had not yet said so, and on Windows the two are one moment. So: iconic
/// on the server (read on this file's own connection), still the full size until a poll has
/// delivered `Resized { 0, 0 }`, and 0x0 from then on.
#[test]
#[ignore = "needs an X server with a window manager: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:93"]
fn the_minimised_size_and_the_minimised_event_are_one_moment() {
    let peer = Peer::open_with_a_manager();
    let mut window = Window::new(&WindowDesc::new("omnidroid: wm one moment", 300, 200)).unwrap();
    window.show();
    poll_until(&mut window, "the focus", |e| *e == WindowEvent::FocusChanged { focused: true });
    window.set_minimized(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while peer.wm_state(xid(&window)) != Some(3) {
        assert!(Instant::now() < deadline, "the manager never iconified the window");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(window.client_size().unwrap(), (300, 200), "no event has said minimised yet");
    poll_until(&mut window, "the minimised size", |e| *e == WindowEvent::Resized { width: 0, height: 0 });
    assert_eq!(window.client_size().unwrap(), (0, 0));
}
