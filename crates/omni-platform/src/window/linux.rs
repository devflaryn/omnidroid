//! Linux backend for the window seam: **X11, through Xlib**.
//!
//! One X connection per [`Window`], one top-level X window on it, and a non-blocking drain of the
//! connection's event queue into [`WindowEvent`]s. It runs wherever an X server does -- Xorg,
//! Xvfb, and **Xwayland**, which is how it runs on a Wayland desktop. A native Wayland backend is
//! not written; see "Why X11 first" below.
//!
//! # Why Xlib, and why loaded at run time
//!
//! **Xlib rather than xcb**, for three reasons that are each a requirement of the seam:
//!
//! * **Typed text needs an input method**, and Xlib has one built in: `XOpenIM` + `XCreateIC` +
//!   `Xutf8LookupString` resolve the layout, dead keys and compose sequences (`^` then `e` is one
//!   `ê`) exactly as every other X client on the desktop does, and hand an IME's committed text
//!   over through the same call. That is the host doing the translation, which is what
//!   [`WindowEvent::Text`] is specified as. With xcb the same needs `libxkbcommon`,
//!   `libxkbcommon-x11` and a compose table of this crate's own choosing, and still no IME.
//! * **Auto-repeat has a server-side switch** in Xlib's XKB half, `XkbSetDetectableAutoRepeat`,
//!   after which a held key is a run of presses and one release rather than release/press pairs a
//!   client has to un-pair by timestamp.
//! * **The Vulkan probe already prefers it.** `omni-gfx`'s `PLATFORM_SURFACE_EXTENSIONS` lists
//!   `VK_KHR_xlib_surface` before `VK_KHR_xcb_surface`, so on a loader that has both (Mesa has
//!   both) the guest's `vkCreateAndroidSurfaceKHR` is rewritten to `vkCreateXlibSurfaceKHR`. A
//!   window handed over as an xcb connection would make the call the rewrite names and the call
//!   the surface is made with two different ones.
//!
//! **Loaded with `dlopen`, not linked** (`x11-dl`, MIT): a Linux machine with no X libraries
//! still builds and runs everything that is not a window, and asking for a window there is a
//! typed refusal naming the library rather than a loader error before `main`. This is the shape
//! `omni-gfx` already has for Vulkan (`ash`'s `loaded`).
//!
//! # The things about this file that are not obvious
//!
//! **1. Closing is a request, because `WM_DELETE_WINDOW` is advertised.** An X window manager
//! closes a window that lists `WM_DELETE_WINDOW` in `WM_PROTOCOLS` by *sending it a message*, and
//! one that does not by **killing the client's connection** (`XKillClient`) -- which for this
//! process is Xlib's I/O error handler and `exit(1)`. So the atom is always advertised, the
//! message becomes [`WindowEvent::CloseRequested`], and [`Window::request_close`] sends the same
//! message to the window itself, so that a programmatic close and the title-bar button are one
//! code path.
//!
//! **2. Protocol errors are recorded, not fatal.** Xlib's default error handler prints and calls
//! `exit`, so a `BadMatch` from asking for the focus a moment too early would end the process.
//! One process-wide handler ([`on_x_error`]) records the error for the thread that caused it, and
//! the operations that can fail check it after an `XSync` ([`Window::checked`]). The I/O error
//! handler -- the connection itself is gone, because the server died -- is left as Xlib's: the
//! window cannot outlive its server, and nothing here could report on it afterwards.
//!
//! **3. The size is ICCCM's, minimised included.** X has no "minimised" in the core protocol; a
//! window manager iconifies a window by unmapping it and writing `IconicState` into its `WM_STATE`
//! property. So `WM_STATE` is watched, and while it says iconic the seam's size is 0x0, as a
//! minimised Win32 window's client area is. Without a window manager nothing can iconify a window
//! -- ICCCM gives the job to the manager -- and [`Window::set_minimized`] refuses by name.
//!
//! **4. A capture is a grab, a blank cursor and XInput 2 raw motion.** `XGrabPointer` confined to
//! the window with an invisible cursor is the hidden, held pointer; `XI_RawMotion` selected on the
//! root window is the device's own unaccelerated motion, which keeps arriving at the confinement's
//! edge. Losing the keyboard focus ends it ([`WindowEvent::PointerCaptureLost`]), as does the
//! window being unmapped, which also ends the server's grab. The pointer is warped back to where it
//! was held when the capture ends, which is where the Windows backend's one-pixel clip leaves it.
//!
//! **Another client can lift the confinement without taking the grab**, MEASURED on this port's
//! Xvfb: a second client's `XGrabPointer` with no `confine_to` fails with `AlreadyGrabbed` -- and
//! the server has already released the holder's confinement by then (`ProcGrabPointer` confines to
//! the root before it tries the grab), so the pointer leaves the window with the grab still held.
//! Re-grabbing puts the confinement back, so [`Window::poll`] does that while a capture is held --
//! the X11 form of the Windows backend re-applying a clip the system reset.
//!
//! **5. The physical key is reported as Windows reports it.** See [`keymap`].
//!
//! # Why X11 first
//!
//! One backend that reaches every Linux desktop -- a Wayland desktop through Xwayland -- rather
//! than two that each reach half. A native Wayland window would add fractional scaling and avoid
//! Xwayland's copy, and is not needed for correctness; it is not written.

use core::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use core::ptr;
use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use x11_dl::xinput2::{self, XInput2};
use x11_dl::xlib::{self, Xlib};

use super::{RawWindow, WindowDesc, WindowError, WindowEvent, WindowResult, push_event};

mod decode;
pub(super) mod keymap;

use decode::{Axis, Button};

// Constants x11-dl does not carry, with the header each comes from.
/// `XkbUseCoreKbd`, `X11/extensions/XKB.h`.
const XKB_USE_CORE_KBD: c_uint = 0x0100;
/// `XkbKeycodesNameMask`, `X11/extensions/XKB.h`.
const XKB_KEYCODES_NAME_MASK: c_uint = 1 << 0;
/// `IconicState`, `X11/Xutil.h` (ICCCM 4.1.3.1).
const ICONIC_STATE: c_long = 3;
/// `NormalState`, `X11/Xutil.h`.
const NORMAL_STATE: c_int = 1;
/// `XA_RESOURCE_MANAGER` and `XA_STRING`, `X11/Xatom.h`.
const XA_RESOURCE_MANAGER: xlib::Atom = 23;
/// `XA_STRING`.
const XA_STRING: xlib::Atom = 31;
/// `XA_CARDINAL`.
const XA_CARDINAL: xlib::Atom = 6;
/// `XA_WM_NAME`.
const XA_WM_NAME: xlib::Atom = 39;
/// `XA_WM_CLASS`.
const XA_WM_CLASS: xlib::Atom = 67;
/// `QueuedAfterFlush`, `X11/Xlib.h`.
const QUEUED_AFTER_FLUSH: c_int = 2;
/// The XInput 2 version this backend asks for. **2.1 is the floor**: from it on, raw events are
/// delivered to a client that selected them on the root window whether or not a grab is active,
/// and the capture holds a grab.
const XI_VERSION: (c_int, c_int) = (2, 2);

/// The libraries, opened once per process.
struct Libs {
    xlib: Xlib,
    /// `libXi`, for pointer capture's raw motion. `None` when the library is missing, which makes
    /// a capture a refusal and nothing else.
    xi: Option<XInput2>,
}

/// Open the libraries, once, and do the two process-wide things Xlib needs done before any
/// connection exists: `XInitThreads` (the Vulkan driver presents to this connection from its own
/// threads) and the error handler of this module's point 2.
fn libs() -> Result<&'static Libs, String> {
    static LIBS: OnceLock<Result<Libs, String>> = OnceLock::new();
    LIBS.get_or_init(|| {
        let xlib = Xlib::open().map_err(|err| err.to_string())?;
        // SAFETY: `XInitThreads` takes nothing and is documented as the first Xlib call a
        // multi-threaded client makes, which it is: this `OnceLock` runs before any `XOpenDisplay`
        // here. `XSetErrorHandler` installs a handler that is valid for the process's lifetime.
        unsafe {
            (xlib.XInitThreads)();
            (xlib.XSetErrorHandler)(Some(on_x_error));
        }
        Ok(Libs { xlib, xi: XInput2::open().ok() })
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// One X protocol error, as the handler saw it.
#[derive(Debug, Clone, Copy)]
struct XErrorRecord {
    code: u8,
    request: u8,
    minor: u8,
    resource: c_ulong,
}

thread_local! {
    /// The last protocol error Xlib reported on this thread. Per thread because Xlib calls the
    /// handler on the thread that read the error off the connection, which for a checked request
    /// is the thread that made it and then called `XSync`.
    static X_ERROR: Cell<Option<XErrorRecord>> = const { Cell::new(None) };
}

/// The process-wide protocol error handler: record, and return, which Xlib then treats as handled.
/// See this module's point 2.
unsafe extern "C" fn on_x_error(_display: *mut xlib::Display, event: *mut xlib::XErrorEvent) -> c_int {
    // SAFETY: Xlib passes a pointer to the error it is reporting, live for this call.
    let event = unsafe { &*event };
    X_ERROR.with(|slot| {
        slot.set(Some(XErrorRecord {
            code: event.error_code,
            request: event.request_code,
            minor: event.minor_code,
            resource: event.resourceid,
        }));
    });
    0
}

/// A typed X11 failure.
fn x11(operation: &'static str, api: &'static str, detail: impl Into<String>) -> WindowError {
    WindowError::X11 { operation, api, detail: detail.into() }
}

/// The atoms this backend uses, interned once per connection.
#[derive(Debug, Clone, Copy)]
struct Atoms {
    wm_protocols: xlib::Atom,
    wm_delete_window: xlib::Atom,
    wm_state: xlib::Atom,
    net_wm_name: xlib::Atom,
    net_wm_pid: xlib::Atom,
    net_active_window: xlib::Atom,
    utf8_string: xlib::Atom,
    /// `WM_S<screen>`, whose owner is the window manager (ICCCM 2.8).
    wm_selection: xlib::Atom,
}

/// One pointing device's two axes, as [`Window::raw_motion`] turns them into motion.
struct Device {
    source: c_int,
    axes: [Axis; 2],
}

/// An X11 window and its own connection.
pub(super) struct Window {
    libs: &'static Libs,
    display: *mut xlib::Display,
    screen: c_int,
    root: xlib::Window,
    /// The window, or 0 until it exists (so that [`Window::drop`] can tear down a creation that
    /// failed part-way).
    window: xlib::Window,
    atoms: Atoms,
    im: xlib::XIM,
    ic: xlib::XIC,
    blank_cursor: xlib::Cursor,
    /// The XInput extension's major opcode, when the server has XInput 2.1 or later.
    xi_opcode: Option<c_int>,
    /// Whether the keymap's keycodes are the `evdev` set (see [`keymap`]).
    evdev_keycodes: bool,
    /// Events produced and not yet drained.
    queue: Vec<WindowEvent>,
    /// The size the window has, from the last `ConfigureNotify` (or the creation-time measurement).
    size: (u32, u32),
    /// The last size reported as a [`WindowEvent::Resized`]: 0x0 while iconic. Starts at
    /// `u32::MAX` so that the first always reports, as the Windows backend's does.
    last_size: (u32, u32),
    /// Whether `WM_STATE` says iconic (this module's point 3).
    iconic: bool,
    /// Bit `k` is set while X keycode `k` is down: what tells an auto-repeat from a press.
    keys_down: [u64; 4],
    /// Whether this window has the keyboard focus, from the focus events that count.
    focused: bool,
    /// Set by [`Window::show`] until the window is mapped, when the focus is asked for. A `Cell`
    /// because `show` takes `&self`.
    focus_on_map: Cell<bool>,
    /// The client-area point the pointer is held at while captured.
    captured: Option<(i32, i32)>,
    /// The pointing devices raw motion has come from during this capture.
    devices: Vec<Device>,
}

impl Window {
    /// Open a connection, create the window on it, and set up everything the seam's events need.
    pub(super) fn create(desc: &WindowDesc<'_>) -> WindowResult<Self> {
        const OP: &str = "create";
        let libs = libs().map_err(|detail| {
            x11(OP, "dlopen(libX11.so.6)", format!("{detail}: this host has no X11 client library"))
        })?;
        let xl = &libs.xlib;
        // SAFETY: a null name asks for `$DISPLAY`.
        let display = unsafe { (xl.XOpenDisplay)(ptr::null()) };
        if display.is_null() {
            let named = std::env::var("DISPLAY").unwrap_or_else(|_| "<unset>".to_owned());
            let wayland = std::env::var("WAYLAND_DISPLAY").ok();
            return Err(x11(
                OP,
                "XOpenDisplay",
                format!(
                    "could not connect to the X server $DISPLAY names ({named:?}){}",
                    match wayland {
                        Some(w) => format!(
                            "; $WAYLAND_DISPLAY is {w:?}, but this backend reaches a Wayland \
                             desktop only through Xwayland, which $DISPLAY must name"
                        ),
                        None => String::new(),
                    }
                ),
            ));
        }
        // SAFETY: a live display.
        let (screen, root) = unsafe {
            let screen = (xl.XDefaultScreen)(display);
            (screen, (xl.XRootWindow)(display, screen))
        };
        let intern = |name: &str| {
            let name = CString::new(name).expect("atom names are literals without NUL");
            // SAFETY: a live display and a NUL-terminated name.
            unsafe { (xl.XInternAtom)(display, name.as_ptr(), xlib::False) }
        };
        let atoms = Atoms {
            wm_protocols: intern("WM_PROTOCOLS"),
            wm_delete_window: intern("WM_DELETE_WINDOW"),
            wm_state: intern("WM_STATE"),
            net_wm_name: intern("_NET_WM_NAME"),
            net_wm_pid: intern("_NET_WM_PID"),
            net_active_window: intern("_NET_ACTIVE_WINDOW"),
            utf8_string: intern("UTF8_STRING"),
            wm_selection: intern(&format!("WM_S{screen}")),
        };
        // From here on, `Drop` tears down whatever has been built.
        let mut window = Window {
            libs,
            display,
            screen,
            root,
            window: 0,
            atoms,
            im: ptr::null_mut(),
            ic: ptr::null_mut(),
            blank_cursor: 0,
            xi_opcode: None,
            evdev_keycodes: false,
            queue: Vec::new(),
            size: (0, 0),
            last_size: (u32::MAX, u32::MAX),
            iconic: false,
            keys_down: [0; 4],
            focused: false,
            focus_on_map: Cell::new(false),
            captured: None,
            devices: Vec::new(),
        };

        // Auto-repeat as presses (this module's header, "Why Xlib").
        let mut supported = xlib::False;
        // SAFETY: a live display and a writable flag.
        unsafe { (xl.XkbSetDetectableAutoRepeat)(display, xlib::True, &raw mut supported) };
        if supported == xlib::False {
            return Err(x11(
                OP,
                "XkbSetDetectableAutoRepeat",
                "the X server does not support detectable auto-repeat, so a held key could not be \
                 told from a key pressed again",
            ));
        }
        window.evdev_keycodes = window.keycodes_name().is_some_and(|n| keymap::is_evdev_keycodes(&n));
        window.xi_opcode = window.xinput2();

        // The window. No background (`background_pixmap` None): the swapchain owns every pixel,
        // and a background would be a flash of it before every expose.
        let event_mask = xlib::KeyPressMask
            | xlib::KeyReleaseMask
            | xlib::ButtonPressMask
            | xlib::ButtonReleaseMask
            | xlib::PointerMotionMask
            | xlib::StructureNotifyMask
            | xlib::FocusChangeMask
            | xlib::PropertyChangeMask;
        // SAFETY: an all-zero `XSetWindowAttributes` is a valid value; only the fields named in
        // the value mask are read.
        let mut attributes: xlib::XSetWindowAttributes = unsafe { core::mem::zeroed() };
        attributes.event_mask = event_mask;
        attributes.background_pixmap = 0;
        let mut created = 0;
        window.checked(OP, "XCreateWindow", |xl| {
            // SAFETY: a live display and root; `validate` has bounded both axes to 1..=65535,
            // which is X's CARD16 range; the attributes live across the call.
            created = unsafe {
                (xl.XCreateWindow)(
                    display,
                    root,
                    0,
                    0,
                    desc.width,
                    desc.height,
                    0,
                    0, // CopyFromParent depth
                    xlib::InputOutput as c_uint,
                    ptr::null_mut(), // CopyFromParent visual
                    xlib::CWEventMask | xlib::CWBackPixmap,
                    &raw mut attributes,
                )
            };
        })?;
        window.window = created;
        window.set_properties(desc.title)?;
        window.open_input_method(event_mask)?;
        window.blank_cursor = window.make_blank_cursor();

        // The initial size, **measured**, in the queue before the first poll -- where the Windows
        // backend's creation-time `WM_SIZE` puts it.
        let (width, height) = window.geometry(OP)?;
        window.size = (width, height);
        window.report_size();
        Ok(window)
    }

    /// Run `request`, then `XSync`, and answer the protocol error it caused, if any.
    fn checked(
        &self,
        operation: &'static str,
        api: &'static str,
        request: impl FnOnce(&Xlib),
    ) -> WindowResult<()> {
        let xl = &self.libs.xlib;
        // SAFETY: a live display. The first sync drains errors from earlier, unchecked requests so
        // that they are not blamed on this one.
        unsafe { (xl.XSync)(self.display, xlib::False) };
        X_ERROR.with(|slot| slot.set(None));
        request(xl);
        // SAFETY: as above.
        unsafe { (xl.XSync)(self.display, xlib::False) };
        match X_ERROR.with(Cell::take) {
            None => Ok(()),
            Some(error) => Err(x11(operation, api, self.describe(error))),
        }
    }

    /// `XGetErrorText` of a recorded error, with the numbers that identify the request.
    fn describe(&self, error: XErrorRecord) -> String {
        let mut text = [0 as c_char; 256];
        // SAFETY: a live display and a buffer whose length is passed.
        unsafe {
            (self.libs.xlib.XGetErrorText)(
                self.display,
                c_int::from(error.code),
                text.as_mut_ptr(),
                text.len() as c_int,
            );
        }
        // SAFETY: `XGetErrorText` NUL-terminates within the length it was given.
        let text = unsafe { CStr::from_ptr(text.as_ptr()) }.to_string_lossy();
        format!(
            "X error {} ({text}) on request {}.{} for resource {:#x}",
            error.code, error.request, error.minor, error.resource
        )
    }

    /// The name of the keymap's keycodes section, e.g. `evdev+aliases(qwerty)`.
    fn keycodes_name(&self) -> Option<String> {
        let xl = &self.libs.xlib;
        // SAFETY: `XkbAllocKeyboard` returns an owned description (freed below) whose
        // `device_spec` is the core keyboard; `XkbGetNames` fills its `names`; the atom name is
        // an Xlib allocation freed with `XFree`.
        unsafe {
            let desc = (xl.XkbAllocKeyboard)();
            if desc.is_null() {
                return None;
            }
            (*desc).device_spec = XKB_USE_CORE_KBD as u16;
            let mut name = None;
            if (xl.XkbGetNames)(self.display, XKB_KEYCODES_NAME_MASK, desc) == 0
                && !(*desc).names.is_null()
                && (*(*desc).names).keycodes != 0
            {
                let atom = (xl.XGetAtomName)(self.display, (*(*desc).names).keycodes);
                if !atom.is_null() {
                    name = Some(CStr::from_ptr(atom).to_string_lossy().into_owned());
                    (xl.XFree)(atom.cast());
                }
            }
            (xl.XkbFreeKeyboard)(desc, 0, xlib::True);
            name
        }
    }

    /// The XInput extension's opcode, when the server speaks XInput 2.1 or later.
    fn xinput2(&self) -> Option<c_int> {
        let xi = self.libs.xi.as_ref()?;
        let xl = &self.libs.xlib;
        let name = c"XInputExtension";
        let (mut opcode, mut event, mut error) = (0, 0, 0);
        // SAFETY: a live display, a NUL-terminated name and three writable integers.
        let present = unsafe {
            (xl.XQueryExtension)(self.display, name.as_ptr(), &raw mut opcode, &raw mut event, &raw mut error)
        };
        if present == 0 {
            return None;
        }
        let (mut major, mut minor) = XI_VERSION;
        // SAFETY: a live display and two writable integers.
        let status = unsafe { (xi.XIQueryVersion)(self.display, &raw mut major, &raw mut minor) };
        (status == xlib::Success as c_int && (major, minor) >= (2, 1)).then_some(opcode)
    }

    /// Title, class, `WM_PROTOCOLS`, hints and pid: what a window manager reads.
    fn set_properties(&self, title: &str) -> WindowResult<()> {
        let (display, window, atoms) = (self.display, self.window, self.atoms);
        self.checked("create", "XChangeProperty", |xl| {
            let title = title.as_bytes();
            let class = b"omnidroid\0Omnidroid\0";
            let pid = [c_long::from(std::process::id() as i32)];
            let mut protocols = [atoms.wm_delete_window];
            // SAFETY: a zeroed `XWMHints` is valid; only the flagged fields are read.
            let mut hints: xlib::XWMHints = unsafe { core::mem::zeroed() };
            hints.flags = xlib::InputHint | xlib::StateHint;
            hints.input = xlib::True;
            hints.initial_state = NORMAL_STATE;
            // SAFETY: a live display and window; every buffer lives across its call and its
            // length is passed; format-32 data is an array of `long`, as Xlib requires.
            unsafe {
                for property in [XA_WM_NAME, atoms.net_wm_name] {
                    (xl.XChangeProperty)(
                        display,
                        window,
                        property,
                        atoms.utf8_string,
                        8,
                        xlib::PropModeReplace,
                        title.as_ptr(),
                        title.len() as c_int,
                    );
                }
                (xl.XChangeProperty)(
                    display,
                    window,
                    XA_WM_CLASS,
                    XA_STRING,
                    8,
                    xlib::PropModeReplace,
                    class.as_ptr(),
                    class.len() as c_int,
                );
                (xl.XChangeProperty)(
                    display,
                    window,
                    atoms.net_wm_pid,
                    XA_CARDINAL,
                    32,
                    xlib::PropModeReplace,
                    pid.as_ptr().cast(),
                    1,
                );
                (xl.XSetWMProtocols)(display, window, protocols.as_mut_ptr(), 1);
                (xl.XSetWMHints)(display, window, &raw mut hints);
            }
        })
    }

    /// Open the input method and an input context for this window: the source of
    /// [`WindowEvent::Text`].
    ///
    /// **The locale is the environment's for the duration of `XOpenIM` only.** An input method is
    /// opened in the process's `LC_CTYPE` and keeps it -- its compose table and its encoding are
    /// that locale's -- but a library that set the process locale and left it would change every
    /// `C`-locale function the rest of the process calls. So `LC_CTYPE` is set from the
    /// environment, the input method opened, and the previous locale put back, under a
    /// process-wide lock; toolkits (SDL among them) do the same. `XMODIFIERS` chooses the input
    /// method (`@im=ibus`), and when the one it names is not running, the built-in one
    /// (`@im=none`: the layout and the locale's compose table) is opened instead.
    fn open_input_method(&mut self, event_mask: c_long) -> WindowResult<()> {
        static LOCALE: Mutex<()> = Mutex::new(());
        let xl = &self.libs.xlib;
        let im = {
            let _held = LOCALE.lock().unwrap_or_else(PoisonError::into_inner);
            // SAFETY: `setlocale` with a null locale reads the current one; the string it returns
            // is copied before the next call can overwrite it. The locale calls are serialised by
            // the lock above, and every Xlib call is on a live display.
            unsafe {
                let previous = libc::setlocale(libc::LC_CTYPE, ptr::null());
                let previous = (!previous.is_null()).then(|| CStr::from_ptr(previous).to_owned());
                if libc::setlocale(libc::LC_CTYPE, c"".as_ptr()).is_null()
                    || (xl.XSupportsLocale)() == 0
                {
                    libc::setlocale(libc::LC_CTYPE, c"C".as_ptr());
                }
                (xl.XSetLocaleModifiers)(c"".as_ptr());
                let mut im = (xl.XOpenIM)(self.display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
                if im.is_null() {
                    (xl.XSetLocaleModifiers)(c"@im=none".as_ptr());
                    im = (xl.XOpenIM)(self.display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
                }
                if let Some(previous) = previous {
                    libc::setlocale(libc::LC_CTYPE, previous.as_ptr());
                }
                im
            }
        };
        if im.is_null() {
            return Err(x11(
                "create",
                "XOpenIM",
                "no input method could be opened, not even the built-in one (@im=none), so typed \
                 text could not be delivered",
            ));
        }
        self.im = im;
        // SAFETY: a live input method; the variadic list is name/value pairs of the types
        // `XCreateIC` documents (`XIMStyle` and two `Window`s), terminated by a null pointer.
        let ic = unsafe {
            (xl.XCreateIC)(
                im,
                xlib::XNInputStyle_0.as_ptr(),
                (xlib::XIMPreeditNothing | xlib::XIMStatusNothing) as c_ulong,
                xlib::XNClientWindow_0.as_ptr(),
                self.window,
                xlib::XNFocusWindow_0.as_ptr(),
                self.window,
                ptr::null_mut::<c_void>(),
            )
        };
        if ic.is_null() {
            return Err(x11(
                "create",
                "XCreateIC",
                "the input method refused a context with no preedit or status area",
            ));
        }
        self.ic = ic;
        // The events the input method needs to see as well as this window's own.
        let mut filter: c_long = 0;
        // SAFETY: a live context; `XNFilterEvents` writes one `long`; null terminates the list.
        unsafe {
            (xl.XGetICValues)(ic, xlib::XNFilterEvents_0.as_ptr(), &raw mut filter, ptr::null_mut::<c_void>());
            (xl.XSelectInput)(self.display, self.window, event_mask | filter);
        }
        Ok(())
    }

    /// A 1x1 cursor with no visible pixel: what a captured pointer is shown as.
    fn make_blank_cursor(&self) -> xlib::Cursor {
        let xl = &self.libs.xlib;
        let bits = [0 as c_char; 1];
        // SAFETY: a live display and window; the pixmap is freed once the cursor holds it; the
        // colour is written by nobody and read as black, which a fully masked cursor never shows.
        unsafe {
            let pixmap = (xl.XCreateBitmapFromData)(self.display, self.window, bits.as_ptr(), 1, 1);
            let mut colour: xlib::XColor = core::mem::zeroed();
            let cursor = (xl.XCreatePixmapCursor)(
                self.display,
                pixmap,
                pixmap,
                &raw mut colour,
                &raw mut colour,
                0,
                0,
            );
            (xl.XFreePixmap)(self.display, pixmap);
            cursor
        }
    }

    /// The window's size, asked of the server.
    fn geometry(&self, operation: &'static str) -> WindowResult<(u32, u32)> {
        // SAFETY: a zeroed `XWindowAttributes` is a valid value to be overwritten.
        let mut attributes: xlib::XWindowAttributes = unsafe { core::mem::zeroed() };
        // SAFETY: a live display and window and a writable struct.
        let ok = unsafe {
            (self.libs.xlib.XGetWindowAttributes)(self.display, self.window, &raw mut attributes)
        };
        if ok == 0 {
            return Err(x11(operation, "XGetWindowAttributes", "the server did not answer for the window"));
        }
        Ok((attributes.width.unsigned_abs(), attributes.height.unsigned_abs()))
    }

    /// Whether the window's `WM_STATE` says `IconicState`, read from the server.
    fn read_iconic(&self) -> bool {
        self.wm_state() == Some(ICONIC_STATE)
    }

    /// The state in the window's `WM_STATE` (ICCCM 4.1.3.1), read from the server: `None` while
    /// no window manager has taken the window on, which is also what it is with no manager at all.
    fn wm_state(&self) -> Option<c_long> {
        let xl = &self.libs.xlib;
        let (mut kind, mut format, mut count, mut after) = (0, 0, 0, 0);
        let mut data: *mut u8 = ptr::null_mut();
        // SAFETY: a live display and window; the out-parameters are writable; `data` is an Xlib
        // allocation freed below, holding `count` format-32 items (`long`s) when non-null.
        unsafe {
            let status = (xl.XGetWindowProperty)(
                self.display,
                self.window,
                self.atoms.wm_state,
                0,
                2,
                xlib::False,
                self.atoms.wm_state,
                &raw mut kind,
                &raw mut format,
                &raw mut count,
                &raw mut after,
                &raw mut data,
            );
            let state = (status == xlib::Success as c_int && !data.is_null() && format == 32 && count >= 1)
                .then(|| *data.cast::<c_long>());
            if !data.is_null() {
                (xl.XFree)(data.cast());
            }
            state
        }
    }

    /// Whether a window manager runs on this screen: the owner of `WM_S<screen>` (ICCCM 2.8).
    fn window_manager_running(&self) -> bool {
        // SAFETY: a live display and an interned atom.
        unsafe { (self.libs.xlib.XGetSelectionOwner)(self.display, self.atoms.wm_selection) != 0 }
    }

    /// Queue a [`WindowEvent::Resized`] for the size the window has now, if it differs from the
    /// last one reported: 0x0 while iconic.
    fn report_size(&mut self) {
        let now = if self.iconic { (0, 0) } else { self.size };
        if now != self.last_size {
            self.last_size = now;
            push_event(&mut self.queue, WindowEvent::Resized { width: now.0, height: now.1 });
        }
    }

    /// Map the window and raise it; ask for the focus once it is mapped.
    pub(super) fn show(&self) {
        let xl = &self.libs.xlib;
        // SAFETY: a zeroed `XWindowAttributes` is valid to be overwritten; a live display and
        // window.
        let viewable = unsafe {
            let mut attributes: xlib::XWindowAttributes = core::mem::zeroed();
            (xl.XGetWindowAttributes)(self.display, self.window, &raw mut attributes) != 0
                && attributes.map_state == xlib::IsViewable
        };
        // SAFETY: a live display and window.
        unsafe { (xl.XMapRaised)(self.display, self.window) };
        if viewable {
            // Already on screen: activate now, as `ShowWindow(SW_SHOWNORMAL)` does.
            self.request_focus();
        } else {
            self.focus_on_map.set(true);
        }
        // SAFETY: a live display.
        unsafe { (xl.XFlush)(self.display) };
    }

    /// Ask for the keyboard focus: through the window manager (`_NET_ACTIVE_WINDOW`, EWMH) when
    /// there is one, since it owns focus policy; directly (`XSetInputFocus`) when there is not.
    fn request_focus(&self) {
        let xl = &self.libs.xlib;
        if self.window_manager_running() {
            let mut event = self.client_message(self.atoms.net_active_window, [1, 0, 0, 0, 0]);
            // SAFETY: a live display and root, and a fully initialised client message.
            unsafe {
                (xl.XSendEvent)(
                    self.display,
                    self.root,
                    xlib::False,
                    xlib::SubstructureRedirectMask | xlib::SubstructureNotifyMask,
                    &raw mut event,
                );
            }
        } else {
            // SAFETY: a live display and window. A `BadMatch` (not yet viewable) is recorded by
            // this module's handler rather than ending the process.
            unsafe {
                (xl.XSetInputFocus)(self.display, self.window, xlib::RevertToParent, xlib::CurrentTime);
            }
        }
        // SAFETY: a live display.
        unsafe { (xl.XFlush)(self.display) };
    }

    /// A format-32 `ClientMessage` about this window.
    fn client_message(&self, message_type: xlib::Atom, data: [c_long; 5]) -> xlib::XEvent {
        // SAFETY: a zeroed `XClientMessageEvent` is a valid value; every field read is set.
        let mut message: xlib::XClientMessageEvent = unsafe { core::mem::zeroed() };
        message.type_ = xlib::ClientMessage;
        message.display = self.display;
        message.window = self.window;
        message.message_type = message_type;
        message.format = 32;
        for (index, value) in data.into_iter().enumerate() {
            message.data.set_long(index, value);
        }
        xlib::XEvent { client_message: message }
    }

    /// Drain the connection's queue through the event handler, then hand over what it produced.
    pub(super) fn poll(&mut self, sink: &mut Vec<WindowEvent>) {
        let xl = &self.libs.xlib;
        // SAFETY: a live display. `XPending` flushes and reads without blocking.
        while unsafe { (xl.XPending)(self.display) } > 0 {
            // SAFETY: a zeroed `XEvent` is valid to be overwritten; `XNextEvent` fills it, and
            // does not block because `XPending` said an event is queued.
            let mut event: xlib::XEvent = unsafe { core::mem::zeroed() };
            // SAFETY: as above.
            unsafe { (xl.XNextEvent)(self.display, &raw mut event) };
            self.handle(&mut event);
        }
        // A confinement another client lifted under a held capture is put back (point 4).
        if self.captured.is_some() {
            self.grab();
        }
        sink.append(&mut self.queue);
    }

    /// Turn one X event into seam events.
    fn handle(&mut self, event: &mut xlib::XEvent) {
        let xl = &self.libs.xlib;
        let kind = event.get_type();
        // **The key as it arrived, before the input method sees it.** The built-in input method
        // rewrites the event it consumes: the key that completes a compose sequence comes back
        // from `XFilterEvent` with its keycode set to 0 (and is put back, as keycode 0, carrying
        // the composed text). The key was still pressed, so the raw event is read from this copy.
        // SAFETY: for a key event, `key` is the member; for anything else the copy is unused.
        let arrived = unsafe { event.key };
        // Every event goes past the input method first: it may be composing, and its own protocol
        // messages arrive as ordinary events it must consume.
        // SAFETY: a live event; 0 is `None`, "the event's own window".
        let filtered = unsafe { (xl.XFilterEvent)(event, 0) } != 0;
        match kind {
            xlib::KeyPress => self.key_press(arrived, event, filtered),
            xlib::KeyRelease => {
                let key = arrived;
                if key.keycode != 0 {
                    self.set_key_down(key.keycode, false);
                    let (keycode, scancode) = self.key_numbers(&key);
                    push_event(&mut self.queue, WindowEvent::KeyUp { keycode, scancode });
                }
            }
            _ if filtered => {}
            xlib::ButtonPress | xlib::ButtonRelease => {
                // SAFETY: the type says `button` is the member.
                let press = unsafe { event.button };
                let (x, y) = self.captured.unwrap_or((press.x, press.y));
                match decode::button(press.button) {
                    Some(Button::Pointer(button)) => push_event(
                        &mut self.queue,
                        if kind == xlib::ButtonPress {
                            WindowEvent::PointerDown { button, x, y }
                        } else {
                            WindowEvent::PointerUp { button, x, y }
                        },
                    ),
                    // A notch is its press; its release carries nothing.
                    Some(Button::Wheel(dx, dy)) if kind == xlib::ButtonPress => {
                        push_event(&mut self.queue, WindowEvent::Wheel { x, y, dx, dy });
                    }
                    _ => {}
                }
            }
            xlib::MotionNotify => {
                // Not while captured: the cursor is held, and what moves is the raw motion.
                if self.captured.is_none() {
                    // SAFETY: the type says `motion` is the member.
                    let motion = unsafe { event.motion };
                    push_event(&mut self.queue, WindowEvent::PointerMoved { x: motion.x, y: motion.y });
                }
            }
            xlib::ConfigureNotify => {
                // SAFETY: the type says `configure` is the member.
                let configure = unsafe { event.configure };
                if configure.window == self.window {
                    self.size = (configure.width.unsigned_abs(), configure.height.unsigned_abs());
                    self.report_size();
                }
            }
            xlib::PropertyNotify => {
                // SAFETY: the type says `property` is the member.
                let property = unsafe { event.property };
                if property.atom == self.atoms.wm_state {
                    self.iconic = self.read_iconic();
                    self.report_size();
                }
            }
            xlib::ClientMessage => {
                // SAFETY: the type says `client_message` is the member.
                let message = unsafe { event.client_message };
                if message.message_type == self.atoms.wm_protocols
                    && message.format == 32
                    && message.data.get_long(0) as xlib::Atom == self.atoms.wm_delete_window
                {
                    push_event(&mut self.queue, WindowEvent::CloseRequested);
                }
            }
            xlib::FocusIn | xlib::FocusOut => {
                // SAFETY: the type says `focus_change` is the member.
                let focus = unsafe { event.focus_change };
                if counts_as_focus_change(focus.mode, focus.detail) {
                    self.set_focused(kind == xlib::FocusIn);
                }
            }
            xlib::MapNotify => {
                if self.focus_on_map.take() {
                    self.request_focus();
                }
            }
            xlib::UnmapNotify => {
                // Unmapping ends the server's grab (its confinement window is no longer viewable),
                // so it ends the capture.
                if self.captured.is_some() {
                    self.end_capture(false);
                    push_event(&mut self.queue, WindowEvent::PointerCaptureLost);
                }
            }
            xlib::MappingNotify => {
                // SAFETY: the type says `mapping` is the member.
                unsafe { (xl.XRefreshKeyboardMapping)(&raw mut event.mapping) };
            }
            xlib::GenericEvent => self.generic(event),
            _ => {}
        }
    }

    /// A key press: the raw [`WindowEvent::KeyDown`] always (an input method composing still
    /// had the key pressed), and the text it typed when the input method did not keep it.
    fn key_press(&mut self, arrived: xlib::XKeyEvent, event: &mut xlib::XEvent, filtered: bool) {
        // Keycode 0 is an input method's committed text, put back as a key event: text, no key.
        if arrived.keycode != 0 {
            let repeat = self.is_key_down(arrived.keycode);
            self.set_key_down(arrived.keycode, true);
            let (keycode, scancode) = self.key_numbers(&arrived);
            push_event(&mut self.queue, WindowEvent::KeyDown { keycode, scancode, repeat });
        }
        if filtered {
            return;
        }
        // SAFETY: the caller matched `KeyPress`, so `key` is the member.
        let mut key = unsafe { event.key };
        let xl = &self.libs.xlib;
        let mut buffer = vec![0u8; 64];
        let (mut keysym, mut status) = (0, 0);
        loop {
            // SAFETY: a live input context and key event; the buffer's length is passed.
            let length = unsafe {
                (xl.Xutf8LookupString)(
                    self.ic,
                    &raw mut key,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as c_int,
                    &raw mut keysym,
                    &raw mut status,
                )
            };
            if status == xlib::XBufferOverflow {
                buffer.resize(usize::try_from(length).unwrap_or(0).max(buffer.len() * 2), 0);
                continue;
            }
            if status == xlib::XLookupChars || status == xlib::XLookupBoth {
                let bytes = &buffer[..usize::try_from(length).unwrap_or(0).min(buffer.len())];
                if let Ok(committed) = core::str::from_utf8(bytes) {
                    for text in decode::text_events(committed) {
                        push_event(&mut self.queue, WindowEvent::Text { text });
                    }
                }
            }
            return;
        }
    }

    /// `(keycode, scancode)` of a key event: the keysym of the key's first level in the event's
    /// group (the layout's own name for the key, `XK_a` for both `a` and `A`, as a Win32
    /// virtual-key code is `VK_A` for both), and the physical key as [`keymap`] reports it.
    fn key_numbers(&self, key: &xlib::XKeyEvent) -> (u32, u32) {
        // `XkbGroupForCoreState`: bits 13-14 of the state.
        let group = ((key.state >> 13) & 3) as c_int;
        // SAFETY: a live display; the keycode is the event's, which is in 8..=255.
        let keysym = unsafe {
            (self.libs.xlib.XkbKeycodeToKeysym)(self.display, key.keycode as u8, group, 0)
        };
        (keysym as u32, keymap::scancode_of_keycode(key.keycode, self.evdev_keycodes))
    }

    fn is_key_down(&self, keycode: c_uint) -> bool {
        let bit = (keycode & 0xFF) as usize;
        self.keys_down[bit / 64] & (1 << (bit % 64)) != 0
    }

    fn set_key_down(&mut self, keycode: c_uint, down: bool) {
        let bit = (keycode & 0xFF) as usize;
        if down {
            self.keys_down[bit / 64] |= 1 << (bit % 64);
        } else {
            self.keys_down[bit / 64] &= !(1 << (bit % 64));
        }
    }

    /// The focus moved to or from this window. Losing it ends a capture -- reported before the
    /// focus change, as the Windows backend does -- and forgets which keys were down: their
    /// releases go to whichever window has the focus now.
    fn set_focused(&mut self, focused: bool) {
        if focused == self.focused {
            return;
        }
        self.focused = focused;
        let xl = &self.libs.xlib;
        if focused {
            // SAFETY: a live input context.
            unsafe { (xl.XSetICFocus)(self.ic) };
        } else {
            // SAFETY: a live input context.
            unsafe { (xl.XUnsetICFocus)(self.ic) };
            self.keys_down = [0; 4];
            if self.captured.is_some() {
                self.end_capture(true);
                push_event(&mut self.queue, WindowEvent::PointerCaptureLost);
            }
        }
        push_event(&mut self.queue, WindowEvent::FocusChanged { focused });
    }

    /// An XInput 2 event: raw motion, while captured.
    fn generic(&mut self, event: &mut xlib::XEvent) {
        let Some(opcode) = self.xi_opcode else { return };
        let xl = &self.libs.xlib;
        // SAFETY: the type says `generic_event_cookie` is the member.
        let cookie = unsafe { &mut event.generic_event_cookie };
        if cookie.extension != opcode {
            return;
        }
        // SAFETY: a live display and this event's own cookie; the data is freed below.
        if unsafe { (xl.XGetEventData)(self.display, cookie) } == 0 {
            return;
        }
        if cookie.evtype == xinput2::XI_RawMotion && self.captured.is_some() {
            // SAFETY: `XGetEventData` succeeded for an `XI_RawMotion`, whose data is an
            // `XIRawEvent`.
            let raw = unsafe { &*cookie.data.cast::<xinput2::XIRawEvent>() };
            let (dx, dy) = self.raw_motion(raw);
            if (dx, dy) != (0, 0) {
                push_event(&mut self.queue, WindowEvent::PointerMotion { dx, dy });
            }
        }
        // SAFETY: the cookie whose data was fetched above.
        unsafe { (xl.XFreeEventData)(self.display, cookie) };
    }

    /// The motion one raw event carries on axes 0 (x) and 1 (y) of its source device.
    fn raw_motion(&mut self, raw: &xinput2::XIRawEvent) -> (i32, i32) {
        let source = raw.sourceid;
        let index = match self.devices.iter().position(|d| d.source == source) {
            Some(index) => index,
            None => {
                let axes = self.device_axes(source);
                self.devices.push(Device { source, axes });
                self.devices.len() - 1
            }
        };
        let mask_len = usize::try_from(raw.valuators.mask_len).unwrap_or(0);
        if raw.valuators.mask.is_null() || raw.raw_values.is_null() {
            return (0, 0);
        }
        // SAFETY: the server sends `mask_len` bytes of mask, and one raw value per set bit, in
        // bit order; both arrays are live with the event data.
        let mask = unsafe { core::slice::from_raw_parts(raw.valuators.mask, mask_len) };
        let mut motion = [0i32; 2];
        let mut value = 0usize;
        for axis in 0..mask_len * 8 {
            if !xinput2::XIMaskIsSet(mask, axis as i32) {
                continue;
            }
            // SAFETY: `value` counts the set bits seen so far, which the array holds one each of.
            let reading = unsafe { *raw.raw_values.add(value) };
            value += 1;
            if axis < 2 {
                motion[axis] = self.devices[index].axes[axis].motion(reading);
            }
        }
        (motion[0], motion[1])
    }

    /// Whether `source`'s x and y axes are relative or absolute, asked of the server.
    fn device_axes(&self, source: c_int) -> [Axis; 2] {
        let mut axes = [Axis::relative(); 2];
        let Some(xi) = self.libs.xi.as_ref() else { return axes };
        let xl = &self.libs.xlib;
        // SAFETY: a live display.
        let extent = unsafe {
            [
                f64::from((xl.XDisplayWidth)(self.display, self.screen)),
                f64::from((xl.XDisplayHeight)(self.display, self.screen)),
            ]
        };
        let mut count = 0;
        // SAFETY: a live display and a writable count; the result is freed below and its classes
        // are read only within `num_classes`.
        unsafe {
            let info = (xi.XIQueryDevice)(self.display, source, &raw mut count);
            if info.is_null() {
                return axes;
            }
            if count >= 1 {
                let device = &*info;
                for class in 0..usize::try_from(device.num_classes).unwrap_or(0) {
                    let any = *device.classes.add(class);
                    if (*any)._type != xinput2::XIValuatorClass {
                        continue;
                    }
                    let valuator = &*any.cast::<xinput2::XIValuatorClassInfo>();
                    let number = usize::try_from(valuator.number).unwrap_or(usize::MAX);
                    if number < 2 && valuator.mode == xinput2::XIModeAbsolute {
                        axes[number] = Axis::absolute(valuator.min, valuator.max, extent[number]);
                    }
                }
            }
            (xi.XIFreeDeviceInfo)(info);
        }
        axes
    }

    /// Select, or stop selecting, raw motion on the root window.
    fn select_raw_motion(&self, on: bool) -> WindowResult<()> {
        let Some(xi) = self.libs.xi.as_ref() else {
            return Err(x11("set_pointer_capture", "XISelectEvents", "libXi.so.6 is not installed"));
        };
        let mut mask = [0u8; 4];
        if on {
            xinput2::XISetMask(&mut mask, xinput2::XI_RawMotion);
        }
        let (display, root) = (self.display, self.root);
        self.checked("set_pointer_capture", "XISelectEvents", |_| {
            let mut selection = xinput2::XIEventMask {
                deviceid: xinput2::XIAllMasterDevices,
                mask_len: mask.len() as c_int,
                mask: mask.as_mut_ptr(),
            };
            // SAFETY: a live display and root; the mask outlives the call.
            unsafe { (xi.XISelectEvents)(display, root, &raw mut selection, 1) };
        })
    }

    /// See [`super::Window::set_pointer_capture`] and this module's point 4.
    pub(super) fn set_pointer_capture(&mut self, captured: bool) -> WindowResult<bool> {
        const OP: &str = "set_pointer_capture";
        if !captured {
            if self.captured.is_some() {
                self.end_capture(true);
            }
            return Ok(false);
        }
        if self.captured.is_some() {
            return Ok(true);
        }
        if !self.focused {
            return Ok(false);
        }
        if self.xi_opcode.is_none() {
            return Err(x11(
                OP,
                "XIQueryVersion",
                "the X server does not speak XInput 2.1 or later (or libXi is missing), and raw \
                 relative motion is what a captured pointer reports",
            ));
        }
        let (width, height) = self.geometry(OP)?;
        if self.iconic || width == 0 || height == 0 {
            return Ok(false);
        }
        // **Where the pointer is held: where it is, inside the client area** -- the Windows
        // backend's rule, for its reason (a pointer held outside the window is hidden nowhere).
        let xl = &self.libs.xlib;
        let (mut root_return, mut child) = (0, 0);
        let (mut root_x, mut root_y, mut x, mut y) = (0, 0, 0, 0);
        let mut buttons = 0;
        // SAFETY: a live display and window and writable out-parameters.
        let on_screen = unsafe {
            (xl.XQueryPointer)(
                self.display,
                self.window,
                &raw mut root_return,
                &raw mut child,
                &raw mut root_x,
                &raw mut root_y,
                &raw mut x,
                &raw mut y,
                &raw mut buttons,
            )
        };
        let held = (
            x.clamp(0, i32::try_from(width).unwrap_or(i32::MAX) - 1),
            y.clamp(0, i32::try_from(height).unwrap_or(i32::MAX) - 1),
        );
        if on_screen == 0 || held != (x, y) {
            // SAFETY: a live display and window; the source window is `None`.
            unsafe { (xl.XWarpPointer)(self.display, 0, self.window, 0, 0, 0, 0, held.0, held.1) };
        }
        self.select_raw_motion(true)?;
        let status = self.grab();
        if status != xlib::GrabSuccess {
            let _ = self.select_raw_motion(false);
            return Err(x11(OP, "XGrabPointer", format!("{} ({status})", grab_status_name(status))));
        }
        self.captured = Some(held);
        self.devices.clear();
        Ok(true)
    }

    /// `XGrabPointer` for a capture: this window receives the grab's events and confines the
    /// pointer, and the cursor is the blank one. Called again from [`Window::poll`] while a capture
    /// is held, because re-grabbing a grab this client holds re-applies its confinement.
    fn grab(&self) -> c_int {
        // SAFETY: a live display, window and cursor.
        unsafe {
            (self.libs.xlib.XGrabPointer)(
                self.display,
                self.window,
                xlib::False,
                (xlib::ButtonPressMask | xlib::ButtonReleaseMask | xlib::PointerMotionMask) as c_uint,
                xlib::GrabModeAsync,
                xlib::GrabModeAsync,
                self.window,
                self.blank_cursor,
                xlib::CurrentTime,
            )
        }
    }

    /// **End a capture**, whatever ended it: the grab released, raw motion deselected, and -- when
    /// `warp` -- the pointer put back where it was held.
    fn end_capture(&mut self, warp: bool) {
        let Some(held) = self.captured.take() else { return };
        self.devices.clear();
        let xl = &self.libs.xlib;
        // SAFETY: a live display. Ungrabbing when the server already released the grab is a
        // no-op.
        unsafe { (xl.XUngrabPointer)(self.display, xlib::CurrentTime) };
        let _ = self.select_raw_motion(false);
        if warp {
            // SAFETY: a live display and window; the source window is `None`.
            unsafe { (xl.XWarpPointer)(self.display, 0, self.window, 0, 0, 0, 0, held.0, held.1) };
        }
        // SAFETY: a live display.
        unsafe { (xl.XFlush)(self.display) };
    }

    /// Whether the capture is held.
    pub(super) fn has_pointer_capture(&self) -> bool {
        self.captured.is_some()
    }

    /// Wait on the connection's descriptor: `true` when an event is queued or the server has
    /// written something, `false` after `timeout`.
    ///
    /// **The queue is asked first, and after a flush.** Xlib reads events off the socket into its
    /// own queue during any round trip, so a socket with nothing on it can sit beside a queue with
    /// events in it; `XEventsQueued(QueuedAfterFlush)` answers for both without blocking.
    pub(super) fn wait(&self, timeout: Duration) -> bool {
        if !self.queue.is_empty() {
            return true;
        }
        let xl = &self.libs.xlib;
        // SAFETY: a live display.
        if unsafe { (xl.XEventsQueued)(self.display, QUEUED_AFTER_FLUSH) } > 0 {
            return true;
        }
        // SAFETY: a live display.
        let fd = unsafe { (xl.XConnectionNumber)(self.display) };
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            // Rounded **up**, so that a wait never returns before its timeout has passed.
            let millis = remaining.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32;
            let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: one live `pollfd`.
            let ready = unsafe { libc::poll(&raw mut poll, 1, millis) };
            if ready > 0 {
                return true;
            }
            if ready == 0 {
                if Instant::now() >= deadline {
                    return false;
                }
                continue;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                // Documented only for a bad descriptor or memory, neither of which this can pass;
                // a failed wait still waits rather than spinning its caller.
                std::thread::sleep(remaining);
                return false;
            }
        }
    }

    /// The window's size, asked of the server, and 0x0 while iconic.
    ///
    /// **Iconic as of the last poll, not as of this call**, and that is deliberate. Read live, the
    /// server's `WM_STATE` runs ahead of the event stream: the `PropertyNotify` that becomes the
    /// seam's `Resized { 0, 0 }` is already sitting in this connection's queue, unread, when a
    /// round trip here sees the new property -- MEASURED, `ndk_host_window`'s minimise test saw
    /// the window report no pixels while the renderer, fed from the events, had not yet heard, and
    /// still held a swapchain. On Windows the two are one moment (`WM_SIZE` is sent from inside
    /// the minimise), and taking the state the events have delivered keeps them one moment here.
    /// Nothing is lost by it: X sends a `PropertyNotify` for every change of a selected property,
    /// so the state is only ever one poll behind, never wrong. The size itself is still asked of
    /// the server, for the Windows backend's reason (a remembered size can stop being true).
    pub(super) fn client_size(&self) -> WindowResult<(u32, u32)> {
        let size = self.geometry("client_size")?;
        Ok(if self.iconic { (0, 0) } else { size })
    }

    /// The display's DPI: `Xft.dpi` from the root window's resources when the desktop set it --
    /// the scale the user chose, the figure Windows' `GetDpiForWindow` is -- and otherwise the
    /// screen's own, from its size in pixels and millimetres. Both asked of the server now.
    pub(super) fn dpi(&self) -> WindowResult<u32> {
        if let Some(dpi) = self.resources().as_deref().and_then(decode::xft_dpi) {
            return Ok(dpi.round() as u32);
        }
        let xl = &self.libs.xlib;
        // SAFETY: a live display and its default screen.
        let (pixels, millimetres) = unsafe {
            (
                (xl.XDisplayWidth)(self.display, self.screen),
                (xl.XDisplayWidthMM)(self.display, self.screen),
            )
        };
        decode::screen_dpi(pixels, millimetres).ok_or_else(|| {
            x11(
                "dpi",
                "XDisplayWidthMM",
                format!(
                    "the root window has no Xft.dpi resource and the screen reports \
                     {pixels} pixels across {millimetres} mm, which is no size"
                ),
            )
        })
    }

    /// The root window's `RESOURCE_MANAGER` text, read now.
    fn resources(&self) -> Option<String> {
        let xl = &self.libs.xlib;
        let (mut kind, mut format, mut count, mut after) = (0, 0, 0, 0);
        let mut data: *mut u8 = ptr::null_mut();
        // SAFETY: a live display and root; writable out-parameters; `data` is an Xlib allocation
        // of `count` bytes (format 8) freed below.
        unsafe {
            let status = (xl.XGetWindowProperty)(
                self.display,
                self.root,
                XA_RESOURCE_MANAGER,
                0,
                1 << 20,
                xlib::False,
                XA_STRING,
                &raw mut kind,
                &raw mut format,
                &raw mut count,
                &raw mut after,
                &raw mut data,
            );
            let text = (status == xlib::Success as c_int && !data.is_null() && format == 8).then(|| {
                let bytes = core::slice::from_raw_parts(data, usize::try_from(count).unwrap_or(0));
                String::from_utf8_lossy(bytes).into_owned()
            });
            if !data.is_null() {
                (xl.XFree)(data.cast());
            }
            text
        }
    }

    /// `XResizeWindow`, when the size differs. See [`super::Window::set_client_size`]: a request,
    /// which a window manager may adjust, answered by a later `ConfigureNotify`.
    pub(super) fn set_client_size(
        &self,
        width: u32,
        height: u32,
        operation: &'static str,
    ) -> WindowResult<()> {
        if self.geometry(operation)? == (width, height) {
            return Ok(());
        }
        let (display, window) = (self.display, self.window);
        self.checked(operation, "XResizeWindow", |xl| {
            // SAFETY: a live display and window; both axes were validated to 1..=65535.
            unsafe { (xl.XResizeWindow)(display, window, width, height) };
        })
    }

    /// `XIconifyWindow` (ICCCM's `WM_CHANGE_STATE` to the window manager), or map the window
    /// again to restore it (ICCCM 4.1.4: iconic to normal is the client mapping its window).
    pub(super) fn set_minimized(&self, minimized: bool) -> WindowResult<()> {
        let (display, window, screen) = (self.display, self.window, self.screen);
        if !minimized {
            self.checked("set_minimized", "XMapRaised", |xl| {
                // SAFETY: a live display and window.
                unsafe { (xl.XMapRaised)(display, window) };
            })?;
            // **And ask the manager, which is what restores it under EWMH.** ICCCM 4.1.4's map is
            // not enough everywhere: MEASURED under GNOME's Mutter (Xwayland), a map leaves an
            // iconic window `Iconic` and only `_NET_ACTIVE_WINDOW` makes it `Normal` -- the
            // activation Windows' `SW_RESTORE` includes. xfwm4 restores on either.
            // `tests/window_linux_ewmh.rs` is that measurement as a test.
            if self.window_manager_running() {
                self.request_focus();
            }
            return Ok(());
        }
        if !self.window_manager_running() {
            return Err(x11(
                "set_minimized",
                "XIconifyWindow",
                format!(
                    "no window manager owns WM_S{screen} on this display. X11 keeps no minimised \
                     state of its own -- ICCCM 4.1.4 has the client ask the window manager, which \
                     unmaps the window and records IconicState -- so with no manager there is \
                     nobody to ask"
                ),
            ));
        }
        // **A minimise supersedes an activation `show` has not yet asked for.** Asked for once the
        // window is mapped, `_NET_ACTIVE_WINDOW` would reach the manager *after* this minimise
        // and un-minimise the window -- MEASURED under xfwm4: a minimise straight after the first
        // frame came back as focus lost, focus regained, and no iconic state at all.
        // (xfwm4 does iconify a window it has not taken on yet, MEASURED: `XIconifyWindow`
        // straight after `XMapRaised` leaves `WM_STATE` iconic, 3 of 3 runs.)
        self.focus_on_map.set(false);
        let mut sent = 0;
        self.checked("set_minimized", "XIconifyWindow", |xl| {
            // SAFETY: a live display and window and their screen number.
            sent = unsafe { (xl.XIconifyWindow)(display, window, screen) };
        })?;
        if sent == 0 {
            return Err(x11("set_minimized", "XIconifyWindow", "the WM_CHANGE_STATE message could not be sent"));
        }
        Ok(())
    }

    /// Send this window the `WM_DELETE_WINDOW` message a window manager sends for the title-bar
    /// button (this module's point 1).
    pub(super) fn request_close(&self) -> WindowResult<()> {
        let mut event = self.client_message(
            self.atoms.wm_protocols,
            [self.atoms.wm_delete_window as c_long, 0, 0, 0, 0],
        );
        let (display, window) = (self.display, self.window);
        let mut sent = 0;
        self.checked("request_close", "XSendEvent", |xl| {
            // SAFETY: a live display and window and a fully initialised client message. An empty
            // event mask sends it to the window's creator, which is this connection.
            sent = unsafe { (xl.XSendEvent)(display, window, xlib::False, 0, &raw mut event) };
        })?;
        if sent == 0 {
            return Err(x11("request_close", "XSendEvent", "the message could not be converted to wire format"));
        }
        Ok(())
    }

    /// The connection and the window, for `VkXlibSurfaceCreateInfoKHR`.
    pub(super) fn raw(&self) -> RawWindow {
        RawWindow::Xlib { display: self.display as usize, window: self.window as u64 }
    }
}

/// Which focus events are the window gaining or losing the keyboard focus.
///
/// **Not a grab's.** A window manager that grabs the keyboard for a shortcut (`Alt+Tab` held)
/// produces `FocusOut` with mode `NotifyGrab` and gives it back with `NotifyUngrab`; the focus has
/// not moved. **Not an inferior's**, which is the focus moving within this window's own tree, and
/// **not `NotifyPointer`/`NotifyPointerRoot`/`NotifyDetailNone`**, which describe the server's
/// focus-follows-pointer bookkeeping rather than this window being focused.
const fn counts_as_focus_change(mode: c_int, detail: c_int) -> bool {
    let mode_counts = mode == xlib::NotifyNormal || mode == xlib::NotifyWhileGrabbed;
    let detail_counts = matches!(
        detail,
        xlib::NotifyAncestor | xlib::NotifyVirtual | xlib::NotifyNonlinear | xlib::NotifyNonlinearVirtual
    );
    mode_counts && detail_counts
}

/// The name of an `XGrabPointer` status, for the error that quotes it.
const fn grab_status_name(status: c_int) -> &'static str {
    match status {
        xlib::AlreadyGrabbed => "AlreadyGrabbed: another client holds the pointer",
        xlib::GrabInvalidTime => "GrabInvalidTime",
        xlib::GrabNotViewable => "GrabNotViewable: the window is not on screen",
        xlib::GrabFrozen => "GrabFrozen: another client froze the pointer",
        _ => "an undocumented status",
    }
}

impl Drop for Window {
    /// Everything this connection holds, in reverse order of creation, then the connection.
    fn drop(&mut self) {
        self.end_capture(true);
        let xl = &self.libs.xlib;
        // SAFETY: each handle is this window's own and is released once; a zero or null one was
        // never created and is skipped. The connection goes last, and with it anything left.
        unsafe {
            if !self.ic.is_null() {
                (xl.XDestroyIC)(self.ic);
            }
            if !self.im.is_null() {
                (xl.XCloseIM)(self.im);
            }
            if self.blank_cursor != 0 {
                (xl.XFreeCursor)(self.display, self.blank_cursor);
            }
            if self.window != 0 {
                (xl.XDestroyWindow)(self.display, self.window);
            }
            (xl.XCloseDisplay)(self.display);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::PointerButton;

    /// The focus events that count, and the ones that are a grab's or the server's bookkeeping.
    #[test]
    fn only_real_focus_moves_count() {
        for detail in [xlib::NotifyAncestor, xlib::NotifyVirtual, xlib::NotifyNonlinear, xlib::NotifyNonlinearVirtual] {
            assert!(counts_as_focus_change(xlib::NotifyNormal, detail), "detail {detail}");
            assert!(counts_as_focus_change(xlib::NotifyWhileGrabbed, detail), "detail {detail}");
            assert!(!counts_as_focus_change(xlib::NotifyGrab, detail), "a grab, detail {detail}");
            assert!(!counts_as_focus_change(xlib::NotifyUngrab, detail), "an ungrab, detail {detail}");
        }
        for detail in [xlib::NotifyInferior, xlib::NotifyPointer, xlib::NotifyPointerRoot, xlib::NotifyDetailNone] {
            assert!(!counts_as_focus_change(xlib::NotifyNormal, detail), "detail {detail}");
        }
    }

    /// The `PointerButton` import is the seam's, and every one of its variants is a core button
    /// this backend decodes.
    #[test]
    fn every_seam_button_has_a_core_button() {
        for wanted in PointerButton::ALL {
            assert!(
                (1..=9).any(|b| decode::button(b) == Some(Button::Pointer(wanted))),
                "{wanted} has no core button"
            );
        }
    }
}
