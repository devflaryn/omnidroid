//! **The X11 backend against a real X server**, driven with real input: `xdotool`, which types,
//! clicks and moves through the XTEST extension exactly as a device would, and a second X
//! connection of this file's own that plays the parts a window manager and a second client play
//! -- sending the close message, reading back what the server holds, trying to take the pointer.
//!
//! The Linux counterpart of `window_live.rs` (which also runs here, and passes but for its
//! `Win32`-handle test): that file asserts the seam's contract through the seam; this one asserts
//! what only X11 has -- the keysym and set-1 scancode of a real key press, the layout's text, the
//! buttons and wheel as the server numbers them, raw motion under a grab -- and checks what it can
//! against **the server** rather than against the backend's own report of itself (VERIFICATION
//! entry 7).
//!
//! # Where these run
//!
//! On an Xvfb with **no window manager** (the port's is `:92`), started as
//! `Xvfb :92 -screen 0 1920x1080x24`. `window_linux_wm.rs` is the half that needs a manager.
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -p omni-platform --release --test window_linux \
//!     -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` is required, not advised: the keyboard focus, the pointer and the root's
//! resources are one per server, and two of these tests typing at once would type into each
//! other's windows. Gated like `window_live.rs`, and for the same reason (VERIFICATION entry 4): a
//! run that asked for these and cannot have them fails naming what is missing.

#![cfg(target_os = "linux")]

use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_long, c_uint, c_ulong};
use std::process::Command;
use std::time::{Duration, Instant};

use omni_platform::window::{PointerButton, RawWindow, Window, WindowDesc, WindowEvent};
use x11_dl::xlib;

const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

/// Fail, naming what is missing, unless this run asked for real windows and has what they need.
fn require_gate() {
    assert!(
        std::env::var(GATE).is_ok_and(|v| v == "1"),
        "run with --ignored but {GATE} is not 1; these create real X windows and type into them"
    );
    assert!(
        std::env::var("DISPLAY").is_ok_and(|d| !d.is_empty()),
        "{GATE}=1 but $DISPLAY is unset: start an Xvfb (Xvfb :92 -screen 0 1920x1080x24) and name it"
    );
    let xdotool = Command::new("xdotool").arg("version").output();
    assert!(
        xdotool.as_ref().is_ok_and(|o| o.status.success()),
        "xdotool is what drives real input here and it could not be run: {xdotool:?}"
    );
}

/// Run `xdotool` with `args` and require it to succeed.
fn xdo(args: &[&str]) {
    let status = Command::new("xdotool").args(args).status().expect("xdotool runs");
    assert!(status.success(), "xdotool {args:?} failed: {status}");
}

/// Poll until `want` holds of some event, or fail after 5 s naming what did arrive.
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
        window.wait(Duration::from_millis(50));
    }
}

/// Poll for `quiet` and return everything that arrived: for asserting that something did **not**
/// happen, after the thing that would have caused it.
fn drain_for(window: &mut Window, quiet: Duration) -> Vec<WindowEvent> {
    let until = Instant::now() + quiet;
    let mut seen = Vec::new();
    while Instant::now() < until {
        seen.extend(window.poll_events());
        window.wait(Duration::from_millis(20));
    }
    seen.extend(window.poll_events());
    seen
}

/// The X window id of a seam window.
fn xid(window: &Window) -> c_ulong {
    match window.raw() {
        RawWindow::Xlib { window, .. } => window as c_ulong,
        other => panic!("the Linux backend must hand out an Xlib handle, got {other:?}"),
    }
}

/// A window, shown and holding the keyboard focus, with its creation events drained.
fn focused_window(title: &str, width: u32, height: u32) -> Window {
    let mut window = Window::new(&WindowDesc::new(title, width, height)).expect("a window");
    window.show();
    poll_until(&mut window, "the focus after show", |e| *e == WindowEvent::FocusChanged { focused: true });
    let _ = drain_for(&mut window, Duration::from_millis(50));
    window
}

/// This file's own X connection: the window manager's and the other client's side.
struct Peer {
    xl: xlib::Xlib,
    display: *mut xlib::Display,
}

impl Peer {
    fn open() -> Peer {
        let xl = xlib::Xlib::open().expect("libX11");
        // SAFETY: a null name is `$DISPLAY`, which `require_gate` checked.
        let display = unsafe { (xl.XOpenDisplay)(std::ptr::null()) };
        assert!(!display.is_null(), "the test's own connection to $DISPLAY failed");
        Peer { xl, display }
    }

    fn atom(&self, name: &str) -> xlib::Atom {
        let name = CString::new(name).unwrap();
        // SAFETY: a live display and a NUL-terminated name.
        unsafe { (self.xl.XInternAtom)(self.display, name.as_ptr(), xlib::False) }
    }

    fn atom_name(&self, atom: xlib::Atom) -> String {
        // SAFETY: a live display; the name is an Xlib allocation freed here.
        unsafe {
            let name = (self.xl.XGetAtomName)(self.display, atom);
            let owned = CStr::from_ptr(name).to_string_lossy().into_owned();
            (self.xl.XFree)(name.cast());
            owned
        }
    }

    fn root(&self) -> xlib::Window {
        // SAFETY: a live display.
        unsafe { (self.xl.XDefaultRootWindow)(self.display) }
    }

    /// The names of the atoms in `window`'s `WM_PROTOCOLS`, as the server holds them.
    fn wm_protocols(&self, window: c_ulong) -> Vec<String> {
        let mut atoms: *mut xlib::Atom = std::ptr::null_mut();
        let mut count = 0;
        // SAFETY: a live display, a window id and writable out-parameters; the list is freed.
        let names = unsafe {
            let ok = (self.xl.XGetWMProtocols)(self.display, window, &raw mut atoms, &raw mut count);
            assert_ne!(ok, 0, "XGetWMProtocols on {window:#x}");
            let list = std::slice::from_raw_parts(atoms, usize::try_from(count).unwrap()).to_vec();
            (self.xl.XFree)(atoms.cast());
            list
        };
        names.into_iter().map(|atom| self.atom_name(atom)).collect()
    }

    /// Send `window` the `WM_DELETE_WINDOW` message, as a window manager does for its close
    /// button (ICCCM 4.2.8.1): a `ClientMessage` of type `WM_PROTOCOLS`, sent to the window.
    fn send_delete(&self, window: c_ulong) {
        self.send_protocol(window, "WM_DELETE_WINDOW");
    }

    /// Send `window` a `WM_PROTOCOLS` message naming `protocol`.
    fn send_protocol(&self, window: c_ulong, protocol: &str) {
        // SAFETY: a zeroed client message is valid; every field read is set.
        let mut message: xlib::XClientMessageEvent = unsafe { std::mem::zeroed() };
        message.type_ = xlib::ClientMessage;
        message.window = window;
        message.message_type = self.atom("WM_PROTOCOLS");
        message.format = 32;
        message.data.set_long(0, self.atom(protocol) as c_long);
        message.data.set_long(1, xlib::CurrentTime as c_long);
        let mut event = xlib::XEvent { client_message: message };
        // SAFETY: a live display and a fully initialised event.
        unsafe {
            assert_ne!((self.xl.XSendEvent)(self.display, window, xlib::False, 0, &raw mut event), 0);
            (self.xl.XSync)(self.display, xlib::False);
        }
    }

    /// Try to grab the pointer from this connection, and give it straight back: what another
    /// client sees while a capture holds it.
    fn try_grab(&self) -> c_int {
        let root = self.root();
        // SAFETY: a live display and root.
        unsafe {
            let status = (self.xl.XGrabPointer)(
                self.display,
                root,
                xlib::False,
                xlib::ButtonPressMask as c_uint,
                xlib::GrabModeAsync,
                xlib::GrabModeAsync,
                0,
                0,
                xlib::CurrentTime,
            );
            if status == xlib::GrabSuccess {
                (self.xl.XUngrabPointer)(self.display, xlib::CurrentTime);
            }
            (self.xl.XSync)(self.display, xlib::False);
            status
        }
    }

    /// Where the pointer is on the root window.
    fn pointer(&self) -> (i32, i32) {
        let (mut root, mut child) = (0, 0);
        let (mut rx, mut ry, mut wx, mut wy) = (0, 0, 0, 0);
        let mut mask = 0;
        // SAFETY: a live display and root, and writable out-parameters.
        unsafe {
            (self.xl.XQueryPointer)(
                self.display,
                self.root(),
                &raw mut root,
                &raw mut child,
                &raw mut rx,
                &raw mut ry,
                &raw mut wx,
                &raw mut wy,
                &raw mut mask,
            );
        }
        (rx, ry)
    }

    /// `window`'s top-left corner on the root window, and its size.
    fn frame(&self, window: c_ulong) -> (i32, i32, i32, i32) {
        let (mut x, mut y, mut child) = (0, 0, 0);
        // SAFETY: a zeroed attribute struct is valid to overwrite; a live display, window, root.
        unsafe {
            let mut attributes: xlib::XWindowAttributes = std::mem::zeroed();
            assert_ne!((self.xl.XGetWindowAttributes)(self.display, window, &raw mut attributes), 0);
            (self.xl.XTranslateCoordinates)(self.display, window, self.root(), 0, 0, &raw mut x, &raw mut y, &raw mut child);
            (x, y, attributes.width, attributes.height)
        }
    }

    /// Whether the cursor the server shows now has **no visible pixel**, from XFixes -- the
    /// server's own image of it.
    fn cursor_is_invisible(&self) -> bool {
        let fixes = x11_dl::xfixes::Xlib::open().expect("libXfixes");
        let (mut major, minor) = (6, 0);
        // SAFETY: a live display; the image is an Xlib allocation freed here, with
        // `width * height` ARGB pixels (one per `long`).
        unsafe {
            (fixes.XFixesQueryVersion)(self.display, &raw mut major, &raw const minor);
            let image = (fixes.XFixesGetCursorImage)(self.display);
            assert!(!image.is_null(), "XFixesGetCursorImage");
            let pixels = std::slice::from_raw_parts(
                (*image).pixels,
                usize::from((*image).width) * usize::from((*image).height),
            );
            let invisible = pixels.iter().all(|&argb| (argb >> 24) & 0xFF == 0);
            (self.xl.XFree)(image.cast());
            invisible
        }
    }

    /// A window of this connection's, mapped, with the keyboard focus moved to it: another
    /// application taking the focus.
    fn take_focus(&self) -> c_ulong {
        // SAFETY: a live display and root; the window is this connection's own.
        unsafe {
            let window = (self.xl.XCreateSimpleWindow)(self.display, self.root(), 1500, 800, 100, 100, 0, 0, 0);
            (self.xl.XMapRaised)(self.display, window);
            (self.xl.XSync)(self.display, xlib::False);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let mut attributes: xlib::XWindowAttributes = std::mem::zeroed();
                (self.xl.XGetWindowAttributes)(self.display, window, &raw mut attributes);
                if attributes.map_state == xlib::IsViewable {
                    break;
                }
                assert!(Instant::now() < deadline, "the peer's window never became viewable");
                std::thread::sleep(Duration::from_millis(10));
            }
            (self.xl.XSetInputFocus)(self.display, window, xlib::RevertToParent, xlib::CurrentTime);
            (self.xl.XSync)(self.display, xlib::False);
            window
        }
    }

    /// Set, or with `None` delete, the root window's `RESOURCE_MANAGER` text.
    fn set_resources(&self, text: Option<&str>) {
        // SAFETY: a live display and root; the text outlives the call.
        unsafe {
            match text {
                Some(text) => {
                    (self.xl.XChangeProperty)(
                        self.display,
                        self.root(),
                        23, // XA_RESOURCE_MANAGER
                        31, // XA_STRING
                        8,
                        xlib::PropModeReplace,
                        text.as_ptr(),
                        text.len() as c_int,
                    );
                }
                None => {
                    (self.xl.XDeleteProperty)(self.display, self.root(), 23);
                }
            }
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

/// The keys `keys.rs`'s extended table and Windows' Num Lock/Pause swap are about, typed for
/// real: each arrives as a press and a release carrying **its keysym** as `keycode` and **its
/// set-1 code** as `scancode` -- the numbers from the PC keyboard's specification, not from this
/// backend's table.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn real_key_presses_carry_their_keysym_and_their_set_1_scancode() {
    require_gate();
    let mut window = focused_window("omnidroid: keys", 320, 240);
    // (xdotool name, keysym from X11/keysymdef.h, set-1 make code)
    let keys: [(&str, u32, u32); 12] = [
        ("a", 0x61, 0x1E),
        ("w", 0x77, 0x11),
        ("space", 0x20, 0x39),
        ("Escape", 0xFF1B, 0x01),
        ("Control_L", 0xFFE3, 0x1D),
        ("Control_R", 0xFFE4, 0xE01D),
        ("Up", 0xFF52, 0xE048),
        ("Left", 0xFF51, 0xE04B),
        ("Delete", 0xFFFF, 0xE053),
        ("KP_Enter", 0xFF8D, 0xE01C),
        ("Pause", 0xFF13, 0x45),
        ("F12", 0xFFC9, 0x58),
    ];
    for (name, keysym, scancode) in keys {
        xdo(&["key", name]);
        let seen = poll_until(&mut window, name, |e| matches!(e, WindowEvent::KeyUp { keycode, .. } if *keycode == keysym));
        // Only this key's events: xdotool presses a modifier keysym's modifier as well (Control_R
        // arrives with Control_L around it), which is xdotool's doing and not the key's.
        let keyed: Vec<&WindowEvent> = seen
            .iter()
            .filter(|e| {
                matches!(e, WindowEvent::KeyDown { keycode, .. } | WindowEvent::KeyUp { keycode, .. } if *keycode == keysym)
            })
            .collect();
        assert_eq!(
            keyed,
            [
                &WindowEvent::KeyDown { keycode: keysym, scancode, repeat: false },
                &WindowEvent::KeyUp { keycode: keysym, scancode },
            ],
            "{name}: {seen:?}"
        );
    }
}

/// **Shift changes the text and not the key**: the `a` key is keysym `a` with or without it (a
/// Win32 virtual-key code is `VK_A` either way), and what it typed is `A`.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn shift_changes_the_text_and_not_the_keycode() {
    require_gate();
    let mut window = focused_window("omnidroid: shift", 320, 240);
    xdo(&["key", "shift+a"]);
    let seen = poll_until(&mut window, "shift+a released", |e| {
        matches!(e, WindowEvent::KeyUp { keycode: 0xFFE1, .. })
    });
    let a_down = seen.iter().position(|e| *e == WindowEvent::KeyDown { keycode: 0x61, scancode: 0x1E, repeat: false });
    let text = seen.iter().position(|e| *e == WindowEvent::Text { text: "A".to_owned() });
    assert!(
        matches!((a_down, text), (Some(key), Some(text)) if key < text),
        "the key, then its text: {seen:?}"
    );
    assert!(
        seen.contains(&WindowEvent::KeyDown { keycode: 0xFFE1, scancode: 0x2A, repeat: false }),
        "left Shift is make 0x2A: {seen:?}"
    );
}

/// **Control characters are keys, not text**: `h` and `!` (Shift+1) are text, and between them
/// Return, BackSpace, Tab, Escape, Ctrl+a and Delete, each of which `Xutf8LookupString` answers
/// with a control code -- and none of which may be text.
///
/// Only keysyms the `us` layout has: for one it lacks, xdotool maps a spare keycode for the moment
/// it types and restores the map straight after, usually before this client has read the event,
/// so the press arrives with no keysym at all -- a harness artifact, not a key. Non-ASCII text is
/// `the_layout_decides_the_text_and_the_physical_key_decides_the_scancode`, on a layout that has
/// it.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn text_is_the_layouts_and_control_characters_are_not_text() {
    require_gate();
    let mut window = focused_window("omnidroid: text", 320, 240);
    xdo(&["type", "--delay", "30", "h"]);
    xdo(&["key", "--delay", "30", "Return", "BackSpace", "Tab", "Escape", "ctrl+a", "Delete"]);
    xdo(&["type", "--delay", "30", "!"]);
    xdo(&["key", "F1"]);
    let seen = poll_until(&mut window, "F1 released", |e| matches!(e, WindowEvent::KeyUp { keycode: 0xFFBE, .. }));
    let texts: Vec<&str> = seen
        .iter()
        .filter_map(|e| match e {
            WindowEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["h", "!"], "all events: {seen:?}");
    // The control keys still arrived, as keys.
    for keysym in [0xFF0D, 0xFF08, 0xFF09, 0xFF1B] {
        assert!(
            seen.iter().any(|e| matches!(e, WindowEvent::KeyDown { keycode, .. } if *keycode == keysym)),
            "keysym {keysym:#x} was pressed: {seen:?}"
        );
    }
}

/// The server's keyboard layout for the life of this value, `us` again afterwards -- also when the
/// test fails, so that one failure does not leave the display typing French at the next test.
struct Layout;

impl Layout {
    fn set(layout: &str) -> Layout {
        let status = Command::new("setxkbmap").args(["-layout", layout]).status().expect("setxkbmap runs");
        assert!(status.success(), "setxkbmap -layout {layout}: {status}");
        Layout
    }
}

impl Drop for Layout {
    fn drop(&mut self) {
        let _ = Command::new("setxkbmap").args(["-layout", "us"]).status();
    }
}

/// **The layout decides the text and the keysym; the physical key decides the scancode** -- the
/// seam's "Keycodes are raw on purpose", on AZERTY (`setxkbmap fr`): the key that types `a` there
/// is the one QWERTY calls `Q`, so it arrives as keysym `a` with Q's set-1 code `0x10`; `é` is the
/// unshifted `2` key (`0x03`); and **a dead key composes** -- `^` (the key right of `P`, `0x1A`)
/// then `e` is one `ê`, from the locale's compose table through the input method, with the dead
/// key itself typing nothing.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn the_layout_decides_the_text_and_the_physical_key_decides_the_scancode() {
    require_gate();
    // The window first: an Xvfb resets -- keymap included -- when its last client disconnects, so
    // the layout is set while this process holds a connection, or setxkbmap's change would last
    // exactly as long as setxkbmap does.
    let mut window = focused_window("omnidroid: azerty", 320, 240);
    let _azerty = Layout::set("fr");
    xdo(&["key", "--delay", "40", "a", "eacute", "dead_circumflex", "e", "F2"]);
    let seen = poll_until(&mut window, "F2 released", |e| matches!(e, WindowEvent::KeyUp { keycode: 0xFFBF, .. }));
    let texts: Vec<&str> = seen
        .iter()
        .filter_map(|e| match e {
            WindowEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["a", "é", "ê"], "all events: {seen:?}");
    for (keysym, scancode, what) in [
        (0x61, 0x10, "a on AZERTY is the Q key"),
        (0xE9, 0x03, "é is the 2 key"),
        (0xFE52, 0x1A, "dead circumflex is the key right of P"),
        (0x65, 0x12, "e is where it always is"),
    ] {
        assert!(
            seen.contains(&WindowEvent::KeyDown { keycode: keysym, scancode, repeat: false }),
            "{what}: {seen:?}"
        );
    }
}

/// **A held key is presses marked as repeats and one release**, with the server's own
/// auto-repeat (660 ms delay, 25 Hz on a default server) doing the repeating.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn a_held_key_repeats_as_presses_and_is_released_once() {
    require_gate();
    let mut window = focused_window("omnidroid: repeat", 320, 240);
    xdo(&["keydown", "d"]);
    std::thread::sleep(Duration::from_millis(1200));
    xdo(&["keyup", "d"]);
    let seen = poll_until(&mut window, "d released", |e| matches!(e, WindowEvent::KeyUp { keycode: 0x64, .. }));
    let keyed: Vec<&WindowEvent> = seen
        .iter()
        .filter(|e| matches!(e, WindowEvent::KeyDown { keycode: 0x64, .. } | WindowEvent::KeyUp { keycode: 0x64, .. }))
        .collect();
    assert_eq!(keyed.first(), Some(&&WindowEvent::KeyDown { keycode: 0x64, scancode: 0x20, repeat: false }), "{seen:?}");
    assert_eq!(keyed.last(), Some(&&WindowEvent::KeyUp { keycode: 0x64, scancode: 0x20 }), "{seen:?}");
    let middle = &keyed[1..keyed.len() - 1];
    assert!(!middle.is_empty(), "1.2 s held produced no repeat: {seen:?}");
    for event in middle {
        assert_eq!(**event, WindowEvent::KeyDown { keycode: 0x64, scancode: 0x20, repeat: true }, "{seen:?}");
    }
    // And a fresh press after the release is not a repeat.
    xdo(&["key", "d"]);
    let seen = poll_until(&mut window, "d again", |e| matches!(e, WindowEvent::KeyUp { keycode: 0x64, .. }));
    assert!(seen.contains(&WindowEvent::KeyDown { keycode: 0x64, scancode: 0x20, repeat: false }), "{seen:?}");
}

/// **All five buttons and the wheel's four directions**, clicked for real at a known client
/// position: each button a press and a release there, each notch one `Wheel` of 120 signed as the
/// seam signs it (up and right positive), and no release event for a notch.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn every_button_and_every_wheel_direction_arrives_where_it_was_clicked() {
    require_gate();
    let mut window = focused_window("omnidroid: buttons", 400, 300);
    let id = xid(&window).to_string();
    xdo(&["mousemove", "--window", &id, "37", "52"]);
    poll_until(&mut window, "the move", |e| *e == WindowEvent::PointerMoved { x: 37, y: 52 });
    for (number, button) in [
        ("1", PointerButton::Primary),
        ("2", PointerButton::Middle),
        ("3", PointerButton::Secondary),
        ("8", PointerButton::Back),
        ("9", PointerButton::Forward),
    ] {
        xdo(&["click", number]);
        let seen = poll_until(&mut window, &format!("button {number}"), |e| matches!(e, WindowEvent::PointerUp { .. }));
        let pressed: Vec<&WindowEvent> =
            seen.iter().filter(|e| matches!(e, WindowEvent::PointerDown { .. } | WindowEvent::PointerUp { .. })).collect();
        assert_eq!(
            pressed,
            [
                &WindowEvent::PointerDown { button, x: 37, y: 52 },
                &WindowEvent::PointerUp { button, x: 37, y: 52 },
            ],
            "button {number}: {seen:?}"
        );
    }
    for (number, dx, dy) in [("4", 0, 120), ("5", 0, -120), ("6", -120, 0), ("7", 120, 0)] {
        xdo(&["click", number]);
        let seen = poll_until(&mut window, &format!("wheel {number}"), |e| matches!(e, WindowEvent::Wheel { .. }));
        let seen = [seen, drain_for(&mut window, Duration::from_millis(100))].concat();
        let wheel: Vec<&WindowEvent> = seen.iter().filter(|e| matches!(e, WindowEvent::Wheel { .. })).collect();
        assert_eq!(wheel, [&WindowEvent::Wheel { x: 37, y: 52, dx, dy }], "button {number}: {seen:?}");
        assert!(
            !seen.iter().any(|e| matches!(e, WindowEvent::PointerDown { .. } | WindowEvent::PointerUp { .. })),
            "a notch is not a button: {seen:?}"
        );
    }
}

/// **A capture, whole**: declined without the focus; granted with it, the pointer is grabbed (a
/// second client cannot take it), hidden (the server's cursor image has no visible pixel, where it
/// had some before) and confined to the window; relative motion arrives as `PointerMotion` in the
/// device's counts and no `PointerMoved`; a click is at the held point. Released, everything is
/// given back and the pointer is where it was held. Captured again, **another client taking the
/// focus** ends it, reported before the focus change.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn a_capture_grabs_hides_and_confines_reports_raw_motion_and_ends_with_the_focus() {
    require_gate();
    let peer = Peer::open();

    // Never shown, so never focused: declined, and nothing grabbed.
    let mut hidden = Window::new(&WindowDesc::new("omnidroid: unfocused", 200, 200)).unwrap();
    assert!(!hidden.set_pointer_capture(true).unwrap(), "declined without the focus");
    assert!(!hidden.has_pointer_capture());
    assert_eq!(peer.try_grab(), xlib::GrabSuccess, "nothing holds the pointer");
    drop(hidden);

    let mut window = focused_window("omnidroid: capture", 400, 300);
    let id = xid(&window);
    let (left, top, width, height) = peer.frame(id);
    xdo(&["mousemove", "--window", &id.to_string(), "120", "90"]);
    poll_until(&mut window, "the move", |e| *e == WindowEvent::PointerMoved { x: 120, y: 90 });
    assert!(!peer.cursor_is_invisible(), "the capture instrument must see a visible cursor first (entry 19)");

    assert!(window.set_pointer_capture(true).unwrap(), "granted with the focus");
    assert!(window.has_pointer_capture());
    assert_eq!(peer.try_grab(), xlib::AlreadyGrabbed, "the capture holds the pointer");
    assert!(peer.cursor_is_invisible(), "the captured pointer is hidden");

    xdo(&["mousemove_relative", "--", "15", "-7"]);
    xdo(&["mousemove_relative", "--", "4", "3"]);
    let seen = poll_until(&mut window, "raw motion", |e| matches!(e, WindowEvent::PointerMotion { .. }));
    let seen = [seen, drain_for(&mut window, Duration::from_millis(150))].concat();
    let total = seen.iter().fold((0, 0), |(x, y), e| match e {
        WindowEvent::PointerMotion { dx, dy } => (x + dx, y + dy),
        _ => (x, y),
    });
    assert_eq!(total, (19, -4), "the device's counts, summed: {seen:?}");
    assert!(!seen.iter().any(|e| matches!(e, WindowEvent::PointerMoved { .. })), "no absolute moves: {seen:?}");

    // Confined: a jump to the far corner of the screen stays inside the window. **Not trivially**:
    // the `try_grab` above, failing with AlreadyGrabbed, already lifted the confinement on the
    // server (MEASURED; see the backend's point 4), and it is the polls since that put it back.
    // Without them the pointer ends up at (1910, 1070) with the grab still held.
    xdo(&["mousemove", "1910", "1070"]);
    let _ = drain_for(&mut window, Duration::from_millis(100));
    let (px, py) = peer.pointer();
    assert!(
        (left..left + width).contains(&px) && (top..top + height).contains(&py),
        "the pointer ({px}, {py}) left the window at ({left}, {top}) {width}x{height}; the capture \
         is {}held and another client's grab answers {}",
        if window.has_pointer_capture() { "" } else { "not " },
        peer.try_grab()
    );

    xdo(&["click", "1"]);
    let seen = poll_until(&mut window, "a captured click", |e| matches!(e, WindowEvent::PointerUp { .. }));
    assert!(
        seen.contains(&WindowEvent::PointerDown { button: PointerButton::Primary, x: 120, y: 90 }),
        "a click while captured is at the held point: {seen:?}"
    );

    // Released: given back, and the pointer where it was held.
    assert!(!window.set_pointer_capture(false).unwrap());
    assert!(!window.has_pointer_capture());
    assert_eq!(peer.try_grab(), xlib::GrabSuccess, "released");
    assert!(!peer.cursor_is_invisible(), "visible again");
    assert_eq!(peer.pointer(), (left + 120, top + 90), "back where it was held");
    let seen = drain_for(&mut window, Duration::from_millis(100));
    assert!(!seen.contains(&WindowEvent::PointerCaptureLost), "a release the caller made is not reported: {seen:?}");

    // Captured again; then another client takes the focus.
    assert!(window.set_pointer_capture(true).unwrap());
    let _ = drain_for(&mut window, Duration::from_millis(50));
    let _other = peer.take_focus();
    let seen = poll_until(&mut window, "the focus loss", |e| *e == WindowEvent::FocusChanged { focused: false });
    let lost = seen.iter().position(|e| *e == WindowEvent::PointerCaptureLost);
    let unfocused = seen.iter().position(|e| *e == WindowEvent::FocusChanged { focused: false });
    assert!(matches!((lost, unfocused), (Some(a), Some(b)) if a < b), "lost, then unfocused: {seen:?}");
    assert!(!window.has_pointer_capture());
    assert_eq!(peer.try_grab(), xlib::GrabSuccess, "the focus loss released the grab");
    assert!(!window.set_pointer_capture(true).unwrap(), "and a new one is declined without the focus");
}

/// **The close button is a request**: the window lists `WM_DELETE_WINDOW` in its `WM_PROTOCOLS`
/// -- read back from the server -- so a window manager sends a message instead of killing the
/// connection, and that message, sent here by another client exactly as a manager sends it, is a
/// `CloseRequested` with the window still alive afterwards.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn the_window_managers_close_message_is_a_request_and_the_window_survives_it() {
    require_gate();
    let peer = Peer::open();
    let mut window = Window::new(&WindowDesc::new("omnidroid: wm close", 320, 240)).unwrap();
    let _ = window.poll_events().count();
    let id = xid(&window);
    assert!(
        peer.wm_protocols(id).iter().any(|name| name == "WM_DELETE_WINDOW"),
        "WM_PROTOCOLS is {:?}",
        peer.wm_protocols(id)
    );
    // Another protocol's message is not a close.
    peer.send_protocol(id, "WM_TAKE_FOCUS");
    let seen = drain_for(&mut window, Duration::from_millis(200));
    assert!(!seen.contains(&WindowEvent::CloseRequested), "WM_TAKE_FOCUS is not a close: {seen:?}");
    peer.send_delete(id);
    poll_until(&mut window, "the close request", |e| *e == WindowEvent::CloseRequested);
    assert_eq!(window.client_size().unwrap(), (320, 240), "still there");
    let (_, _, width, height) = peer.frame(id);
    assert_eq!((width, height), (320, 240), "and the server agrees");
}

/// **Without a window manager nothing can iconify a window**, and minimising says so by name
/// rather than pretending: ICCCM gives iconic state to the manager.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn minimising_without_a_window_manager_is_refused_by_name() {
    require_gate();
    let peer = Peer::open();
    // SAFETY: a live display and an interned atom.
    let owner = unsafe { (peer.xl.XGetSelectionOwner)(peer.display, peer.atom("WM_S0")) };
    assert_eq!(owner, 0, "this test is for a display with no window manager (the port's :92)");
    let window = focused_window("omnidroid: no wm", 320, 240);
    let error = window.set_minimized(true).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("XIconifyWindow") && text.contains("WM_S0"), "{text}");
    assert_eq!(window.client_size().unwrap(), (320, 240), "nothing happened");
}

/// **The DPI is the server's**: the screen's own figure when no desktop has set `Xft.dpi` --
/// checked against the server's size in pixels and millimetres, read on this file's connection --
/// and `Xft.dpi` when one has, read at the call rather than remembered.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn the_dpi_is_xft_dpi_when_the_desktop_set_it_and_the_screens_otherwise() {
    require_gate();
    let peer = Peer::open();
    peer.set_resources(None);
    let window = Window::new(&WindowDesc::new("omnidroid: dpi", 320, 240)).unwrap();
    // SAFETY: a live display and its default screen.
    let (pixels, millimetres) = unsafe {
        let screen = (peer.xl.XDefaultScreen)(peer.display);
        ((peer.xl.XDisplayWidth)(peer.display, screen), (peer.xl.XDisplayWidthMM)(peer.display, screen))
    };
    let expected = (f64::from(pixels) * 25.4 / f64::from(millimetres)).round() as u32;
    assert_eq!(window.dpi().unwrap(), expected, "{pixels} px across {millimetres} mm");
    peer.set_resources(Some("Xcursor.size:\t24\nXft.dpi:\t144\n"));
    assert_eq!(window.dpi().unwrap(), 144, "a desktop at 150% scaling");
    peer.set_resources(None);
    assert_eq!(window.dpi().unwrap(), expected, "and back");
}

/// **`wait` sleeps until something arrives, and no longer**: nothing queued is `false` after the
/// timeout and not before; a key typed from another process wakes it at once.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn wait_returns_when_input_arrives_and_times_out_without_it() {
    require_gate();
    let mut window = focused_window("omnidroid: wait", 320, 240);
    let started = Instant::now();
    assert!(!window.wait(Duration::from_millis(60)), "nothing was queued");
    assert!(started.elapsed() >= Duration::from_millis(60), "returned early: {:?}", started.elapsed());
    let typist = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(200));
        xdo(&["key", "b"]);
    });
    let started = Instant::now();
    assert!(window.wait(Duration::from_secs(5)), "a key was typed");
    let woke = started.elapsed();
    assert!(woke < Duration::from_secs(2), "slept past it: {woke:?}");
    typist.join().unwrap();
    poll_until(&mut window, "the key", |e| matches!(e, WindowEvent::KeyDown { keycode: 0x62, .. }));
}

/// **The handle is a live Xlib window**: a display pointer and an id the server knows, of the
/// size the seam reports, named `xlib`.
#[test]
#[ignore = "needs an X server and xdotool: OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:92 cargo test -- --ignored"]
fn the_raw_handle_is_a_live_xlib_window() {
    require_gate();
    let peer = Peer::open();
    let window = Window::new(&WindowDesc::new("omnidroid: handle", 333, 222)).unwrap();
    match window.raw() {
        RawWindow::Xlib { display, window: id } => {
            assert_ne!(display, 0, "the Display * is null");
            let (_, _, width, height) = peer.frame(id as c_ulong);
            assert_eq!((width, height), (333, 222), "the server knows the window, at its size");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(window.raw().system_name(), "xlib");
}
