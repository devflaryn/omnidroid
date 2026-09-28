//! Windows backend for the window seam.
//!
//! One window class per process, one `HWND` per [`Window`], and a window procedure whose only job
//! is to turn messages into [`WindowEvent`]s and put them in a queue that
//! [`Window::poll`] later hands over. Nothing here renders, and nothing here blocks.
//!
//! # The four things about this file that are not obvious
//!
//! **1. The state pointer is installed at `WM_NCCREATE`, not after `CreateWindowExW` returns.**
//! Win32 *sends* `WM_NCCREATE`, `WM_CREATE` and `WM_SIZE` to the window procedure from inside
//! `CreateWindowExW`, before it has returned anything to call `SetWindowLongPtrW` with. A backend
//! that installs the pointer afterwards drops every event that arrives during creation — which is
//! the window's initial size, the one event a renderer most needs. The pointer therefore travels
//! as `CREATESTRUCTW::lpCreateParams` and is installed by the first message that carries it.
//!
//! **2. `WM_CLOSE` is swallowed.** `DefWindowProcW`'s handling of `WM_CLOSE` is to call
//! `DestroyWindow`, so letting it through would mean the title-bar button destroying the window
//! under a running guest. This seam's contract is that closing is the *runtime's* decision (see
//! [`WindowEvent::CloseRequested`]), so the message becomes an event and returns 0.
//!
//! **3. The pointer is captured while a button is held.** Without `SetCapture`, `WM_MOUSEMOVE`
//! stops at the client edge, so a drag that leaves the window silently freezes at the border
//! instead of continuing — and [`WindowEvent::PointerMoved`] documents that its coordinates may
//! be outside the client area, which would be a false claim otherwise. Capture is taken on the
//! first button down and released when the last one comes up; `WM_CAPTURECHANGED` resets the
//! bookkeeping if something else takes it away, because a lost capture with a stale mask would
//! leave the window believing a button is still down forever.
//!
//! **4. The requested client size is *measured*, not predicted.** See [`Window::create`].
//!
//! **5. A pointer capture is three things, and all three are undone together.** Android's
//! `requestPointerCapture` hides the pointer, holds it where it is, and hands the app the device's
//! relative motion. Here that is: the cursor clipped to the **one pixel** it is on
//! (`ClipCursor`), so it cannot drift and reappears exactly there; `SetCursor(NULL)` from
//! `WM_SETCURSOR` over the client area; and **raw input** (`RegisterRawInputDevices`, then
//! `WM_INPUT`), whose mouse motion is the device's counts before the pointer ballistics -- and
//! keeps arriving with the cursor pinned, which is the point. Clipping is process-wide and survives
//! this window, so every way out -- a release, the focus leaving (`WM_KILLFOCUS`, reported as
//! [`WindowEvent::PointerCaptureLost`]), the window being dropped -- goes through
//! `end_capture`, which undoes all three. A clip the system resets under a held capture
//! (a secure desktop, `Ctrl+Alt+Del`) is put back at the next [`Window::poll`].
//!
//! **6. A hidden cursor is `WM_SETCURSOR`'s answer, not `ShowCursor`'s counter.** `ShowCursor` is
//! a per-thread display count that outlives any window and has to be balanced exactly; a count
//! left one short hides the cursor over every window of the desktop. `WM_SETCURSOR` is asked by
//! the system, for the window under the cursor -- **active or not** -- every time the cursor moves
//! over it: answering it with `SetCursor(NULL)` over the client area while
//! [`Window::set_cursor_hidden`] asks hides it exactly there and nowhere else, and the frame, the
//! title bar and every other window keep their own cursors, so moving off the window shows it.
//! The request itself, and a capture's end, set the cursor at once when it is over the client
//! area, because `WM_SETCURSOR` would otherwise wait for the next move.
//!
//! **7. A button reported down is reported up, however its release was lost.** The window takes
//! the mouse capture on the first press (point 3), so a release outside the client area still
//! comes here; but another window can take that capture (`WM_CAPTURECHANGED`, which a system menu
//! or `WM_CANCELMODE` also causes), and then every button this window reported down is reported up
//! at once. And while any button is down, [`Window::poll`] asks the host for the physical buttons
//! (`GetAsyncKeyState`, through the swap setting `SM_SWAPBUTTON`) and reports up any that it
//! finds up -- the safety net for a release no message brought.
//!
//! **8. A presented image is painted from `WM_PAINT`, and kept.** [`Window::present_rgba`] and a
//! [`Presenter`] store the image in the window's [`Canvas`] and invalidate the client area;
//! `WM_PAINT` stretches it over the client area with `StretchDIBits` (`Window::present_rgba`,
//! on the window's thread, also calls `UpdateWindow`, which sends that `WM_PAINT` at once).
//! Painting from the message rather than straight into a DC is what makes a [`Presenter`] on
//! another thread live **during a border drag**: the modal loop (see [`super`]'s "Why polling")
//! keeps the window's thread inside `DispatchMessageW`, and it still dispatches `WM_PAINT` for
//! the invalidations the other thread makes -- and `GetClientRect`, which the presenter's
//! `client_size` asks, answers the size the border is at. Every `WM_SIZE` invalidates the whole
//! client area -- the class has no `CS_HREDRAW`/`CS_VREDRAW` -- so the image follows the border
//! instead of leaving the newly exposed strip undrawn.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, COLORONCOLOR, ClientToScreen, DIB_RGB_COLORS,
    EndPaint, HMONITOR, InvalidateRect, MONITOR_DEFAULTTONEAREST, MonitorFromWindow, PAINTSTRUCT,
    SRCCOPY, ScreenToClient, SetStretchBltMode, StretchDIBits, UpdateWindow,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetFocus, ReleaseCapture, SetCapture, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
    VK_XBUTTON1, VK_XBUTTON2,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, MOUSE_VIRTUAL_DESKTOP, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RIDEV_REMOVE, RIM_TYPEMOUSE,
    RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, ClipCursor, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetClientRect, GetCursorPos, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowRect, HTCLIENT, IDC_ARROW, LoadCursorW, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostMessageW, QS_ALLINPUT,
    RegisterClassW, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_SWAPBUTTON, SW_MINIMIZE,
    SW_RESTORE, SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetCursor, SetCursorPos,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, WindowFromPoint, WM_CAPTURECHANGED, WM_CHAR,
    WM_CLOSE, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR,
    WM_SETFOCUS, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_WINDOWPOSCHANGED, WM_XBUTTONDOWN,
    WM_XBUTTONUP, WNDCLASSW, WS_OVERLAPPEDWINDOW, XBUTTON1,
};

use super::{
    DisplayChange, PointerButton, RawWindow, WindowDesc, WindowError, WindowEvent, WindowResult,
    push_event,
};

/// Everything the window procedure owns.
///
/// Lives in its own heap allocation reached **only** through a raw pointer — never through a
/// `Box` field on [`Window`]. That is deliberate: the window procedure builds a `&mut` to it from
/// `GWLP_USERDATA` while [`Window::poll`] holds a `&mut Window`, and if the allocation were owned
/// by a `Box` inside `Window` those two references would alias the same owned value. Reaching it
/// through a raw pointer with its own provenance is what makes that sound.
struct WindowState {
    /// Events the window procedure has produced and nobody has drained yet.
    queue: Vec<WindowEvent>,
    /// The last client size reported as a [`WindowEvent::Resized`].
    ///
    /// Windows sends `WM_SIZE` for things that are not size changes — showing, restoring,
    /// activating — and a [`WindowEvent::Resized`] that names the size the swapchain already has
    /// would cost a swapchain recreation for nothing. The spike measured what swapchain
    /// recreation costs when it is wrong (`docs/research/graphics-spike.md` §1: a driver crash on
    /// every live resize), so a no-op recreation is not a cheap mistake to make repeatedly.
    ///
    /// Starts at `(u32::MAX, u32::MAX)` rather than `(0, 0)` so that the *first* `WM_SIZE` always
    /// reports: `(0, 0)` is a real size, the one a minimised window has.
    last_size: (u32, u32),
    /// Bit `n` is set while the `n`-th [`PointerButton`] is held. Drives `SetCapture`; see this
    /// module's point 3.
    buttons_down: u32,
    /// The first half of a surrogate pair, held until the `WM_CHAR` carrying the second half
    /// arrives. See [`text_from_char`].
    ///
    /// Per window rather than per thread or per process: one thread may own several windows, and
    /// a half character typed into one must not be completed by a `WM_CHAR` sent to another.
    high_surrogate: Option<u16>,
    /// The screen point the cursor is held at while this window has the pointer captured, and
    /// `None` while it does not. See this module's point 5.
    captured: Option<POINT>,
    /// The last position an **absolute** raw-input device reported, in screen pixels, so that its
    /// next report can be turned into motion. See [`raw_motion`].
    last_absolute: Option<(i32, i32)>,
    /// The monitor the window was last seen on, so that a move onto another one is reported as
    /// [`DisplayChange::Monitor`]. Null until the first `WM_WINDOWPOSCHANGED`, which is not a move.
    monitor: HMONITOR,
    /// Whether [`super::Window::set_cursor_hidden`] asks for the cursor to be hidden over the
    /// client area. See this module's point 6.
    hide_cursor: bool,
    /// Whether this window has the keyboard focus, from `WM_SETFOCUS`/`WM_KILLFOCUS`.
    focused: bool,
    /// The pointer's last client position a message carried: where a release this window
    /// reports for a button whose own release was lost (point 7) is.
    last_pointer: (i32, i32),
    /// What the window shows when something is presented to it (point 8), shared with its
    /// [`Presenter`]s.
    canvas: Arc<Mutex<Canvas>>,
}

/// The presented image, shared between the window's thread (which paints it) and any
/// [`Presenter`] (which replaces it).
struct Canvas {
    /// `None` for a window nothing was presented to -- one a swapchain owns -- whose painting is
    /// left to `DefWindowProcW` as before.
    image: Option<Image>,
    /// Cleared, under this lock, before the window is destroyed: a presenter that finds it clear
    /// touches no handle, since the `HWND` may by then name another window.
    alive: bool,
}

/// Lock a canvas; a panic elsewhere while holding it leaves an image, which is still an image.
fn lock(canvas: &Mutex<Canvas>) -> std::sync::MutexGuard<'_, Canvas> {
    canvas.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Store `rgba` in the canvas as GDI's BGRA.
fn store(canvas: &mut Canvas, rgba: &[u8], width: u32, height: u32) {
    let image = canvas.image.get_or_insert_with(|| Image { bgra: Vec::new(), width: 0, height: 0 });
    image.bgra.resize(rgba.len(), 0);
    for (d, s) in image.bgra.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
        d.copy_from_slice(&[s[2], s[1], s[0], 0xff]);
    }
    (image.width, image.height) = (width, height);
}

/// Presents to a window from any thread: see [`super::Presenter`] and point 8.
#[derive(Clone)]
pub(super) struct Presenter {
    /// The `HWND`, as an integer (a handle is not `Send`; the canvas's `alive` says when it is
    /// still this window's).
    hwnd: isize,
    canvas: Arc<Mutex<Canvas>>,
}

impl Presenter {
    /// Replace the image and invalidate the client area; the window's thread paints it at its
    /// next message pump, inside a modal loop too. Nothing once the window is gone.
    pub(super) fn present_rgba(&self, rgba: &[u8], width: u32, height: u32) -> WindowResult<()> {
        let mut canvas = lock(&self.canvas);
        if !canvas.alive {
            return Ok(());
        }
        store(&mut canvas, rgba, width, height);
        // SAFETY: the window is alive (checked under the lock its destruction takes first);
        // `InvalidateRect` may be called from any thread and sends nothing. A null rectangle is
        // the whole client area.
        unsafe { InvalidateRect(self.hwnd as HWND, core::ptr::null(), 0) };
        Ok(())
    }

    /// `GetClientRect`, callable from any thread: the size the border is at, mid-drag included.
    pub(super) fn client_size(&self) -> Option<(u32, u32)> {
        let canvas = lock(&self.canvas);
        if !canvas.alive {
            return None;
        }
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: a live window (checked under the lock); writes a `RECT`.
        (unsafe { GetClientRect(self.hwnd as HWND, &raw mut rect) } != 0).then(|| (rect.right.unsigned_abs(), rect.bottom.unsigned_abs()))
    }
}

/// A presented image, as GDI takes it: bottom-up is GDI's default, so the height in the header is
/// negated to say "top-down", and the bytes are BGRA -- `BI_RGB` at 32 bits a pixel is blue first.
struct Image {
    bgra: Vec<u8>,
    width: u32,
    height: u32,
}

impl WindowState {
    /// Whether the cursor is not drawn over the client area now: captured, or asked to be hidden.
    fn cursor_invisible(&self) -> bool {
        self.captured.is_some() || self.hide_cursor
    }

    /// Report every button held as released, at the last position, and forget them (point 7).
    fn release_all_buttons(&mut self) {
        let (x, y) = self.last_pointer;
        for button in ALL_BUTTONS {
            if self.buttons_down & (1 << button_bit(button)) != 0 {
                push_event(&mut self.queue, WindowEvent::PointerUp { button, x, y });
            }
        }
        self.buttons_down = 0;
    }
}

/// Every button, in `button_bit` order.
const ALL_BUTTONS: [PointerButton; 5] = [
    PointerButton::Primary,
    PointerButton::Secondary,
    PointerButton::Middle,
    PointerButton::Back,
    PointerButton::Forward,
];

/// The virtual key `GetAsyncKeyState` answers a button by: the **physical** button, so the
/// primary and secondary swap with the user's `SM_SWAPBUTTON` setting.
const fn physical_key(button: PointerButton, swapped: bool) -> u16 {
    match (button, swapped) {
        (PointerButton::Primary, false) | (PointerButton::Secondary, true) => VK_LBUTTON,
        (PointerButton::Secondary, false) | (PointerButton::Primary, true) => VK_RBUTTON,
        (PointerButton::Middle, _) => VK_MBUTTON,
        (PointerButton::Back, _) => VK_XBUTTON1,
        (PointerButton::Forward, _) => VK_XBUTTON2,
    }
}

/// Whether the cursor is over `hwnd`'s client area now.
fn cursor_over_client(hwnd: HWND) -> bool {
    let mut at = POINT { x: 0, y: 0 };
    // SAFETY: writes a `POINT`.
    if unsafe { GetCursorPos(&raw mut at) } == 0 {
        return false;
    }
    // SAFETY: by-value point.
    if unsafe { WindowFromPoint(at) } != hwnd {
        return false;
    }
    let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a live window handle; writes a `POINT` and a `RECT`.
    unsafe {
        ScreenToClient(hwnd, &raw mut at);
        GetClientRect(hwnd, &raw mut client);
    }
    (0..client.right).contains(&at.x) && (0..client.bottom).contains(&at.y)
}

/// Set the cursor now to what `WM_SETCURSOR` would answer, when it is over the client area (point
/// 6): nothing while it should be invisible, the class's arrow otherwise. Elsewhere the cursor is
/// another window's business and is left alone.
fn refresh_cursor(hwnd: HWND, state: &WindowState) {
    if !cursor_over_client(hwnd) {
        return;
    }
    let cursor = if state.cursor_invisible() {
        core::ptr::null_mut()
    } else {
        // SAFETY: a system cursor, as `window_class` loads it.
        unsafe { LoadCursorW(core::ptr::null_mut(), IDC_ARROW) }
    };
    // SAFETY: a null cursor hides it; a system cursor handle is live for the process.
    unsafe { SetCursor(cursor) };
}

/// The mouse on the generic-desktop usage page: what [`start_capture`] registers raw input for.
const RAW_MOUSE: (u16, u16) = (0x01, 0x02);

/// Register this window for the mouse's raw input (`register`), or unregister the process from it.
fn register_raw_mouse(hwnd: HWND, register: bool) -> Result<(), u32> {
    let device = RAWINPUTDEVICE {
        usUsagePage: RAW_MOUSE.0,
        usUsage: RAW_MOUSE.1,
        // No `RIDEV_INPUTSINK`: raw input only while this window is in the foreground, which is the
        // only time a capture is held. No `RIDEV_NOLEGACY`: the buttons still arrive as the
        // ordinary messages, with the positions every other button carries.
        dwFlags: if register { 0 } else { RIDEV_REMOVE },
        // `RIDEV_REMOVE` requires a null target.
        hwndTarget: if register { hwnd } else { core::ptr::null_mut() },
    };
    // SAFETY: one fully-initialised `RAWINPUTDEVICE`, its size, and a count of one.
    let ok = unsafe {
        RegisterRawInputDevices(&raw const device, 1, size_of::<RAWINPUTDEVICE>() as u32)
    };
    if ok == 0 {
        // SAFETY: no arguments, and `RegisterRawInputDevices` is the last call this thread made.
        return Err(unsafe { GetLastError() });
    }
    Ok(())
}

/// Confine the cursor to the one pixel at `at`, which pins it there.
fn clip_to(at: POINT) -> Result<(), u32> {
    let rect = RECT { left: at.x, top: at.y, right: at.x + 1, bottom: at.y + 1 };
    // SAFETY: a live `RECT` read for the duration of the call.
    if unsafe { ClipCursor(&raw const rect) } == 0 {
        // SAFETY: no arguments, and `ClipCursor` is the last call this thread made.
        return Err(unsafe { GetLastError() });
    }
    Ok(())
}

/// **End a capture**, whatever ended it: the clip lifted, raw input unregistered, the state
/// cleared. The cursor's visibility is its callers' to set back ([`refresh_cursor`]): it is hidden
/// only from `WM_SETCURSOR` while `captured` is set, and the next one is the next move.
///
/// Best effort, because every caller is already on its way out of the capture and has nothing
/// better to do with a refusal: an unlifted clip would be the one lasting harm, and `ClipCursor`
/// with a null rectangle is documented to succeed.
fn end_capture(state: &mut WindowState) {
    if state.captured.take().is_none() {
        return;
    }
    state.last_absolute = None;
    // SAFETY: a null rectangle lifts the clip; no memory is read.
    unsafe { ClipCursor(core::ptr::null()) };
    let _ = register_raw_mouse(core::ptr::null_mut(), false);
}

/// **Raw mouse motion as a distance**, from one `RAWMOUSE`'s flags and `lLastX`/`lLastY`.
///
/// * **Relative** (`MOUSE_MOVE_RELATIVE`, the flag clear) -- every mouse: the two values *are* the
///   motion, in device counts, before Windows' pointer ballistics.
/// * **Absolute** (`MOUSE_MOVE_ABSOLUTE`) -- a pen tablet, a remote-desktop or streaming client:
///   the two values are a position normalised to `0..=65535` across `extent` (the virtual desktop
///   when `MOUSE_VIRTUAL_DESKTOP` is set, the primary monitor otherwise), so motion is the
///   difference from the last one, in screen pixels. The first absolute report has nothing to
///   differ from and is no motion.
///
/// Total: an extent of zero or less is treated as one pixel, and the arithmetic is 64-bit.
fn raw_motion(
    flags: u16,
    x: i32,
    y: i32,
    last_absolute: &mut Option<(i32, i32)>,
    extent: (i32, i32),
) -> (i32, i32) {
    if flags & MOUSE_MOVE_ABSOLUTE == 0 {
        *last_absolute = None;
        return (x, y);
    }
    let to_pixels = |value: i32, span: i32| (i64::from(value) * i64::from(span.max(1)) / 65535) as i32;
    let now = (to_pixels(x, extent.0), to_pixels(y, extent.1));
    match last_absolute.replace(now) {
        Some((was_x, was_y)) => (now.0.saturating_sub(was_x), now.1.saturating_sub(was_y)),
        None => (0, 0),
    }
}

/// Read the `WM_INPUT` whose handle is `lparam`: the mouse motion it carries, or `None` for
/// something that is not a mouse or could not be read.
fn read_raw_motion(lparam: LPARAM, last_absolute: &mut Option<(i32, i32)>) -> Option<(i32, i32)> {
    let mut raw = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    // SAFETY: `raw` is a `RAWINPUT`, `size` says how large it is, and the handle is the one this
    // message carries, valid until the message is passed to `DefWindowProcW`. A mouse report is
    // exactly `RAWINPUTHEADER` plus `RAWMOUSE`, which a `RAWINPUT` holds.
    let copied = unsafe {
        GetRawInputData(
            lparam as HRAWINPUT,
            RID_INPUT,
            (&raw mut raw).cast(),
            &raw mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if copied == u32::MAX || copied == 0 || raw.header.dwType != RIM_TYPEMOUSE {
        return None;
    }
    // SAFETY: `dwType` is `RIM_TYPEMOUSE`, so `mouse` is the union member the call wrote.
    let mouse = unsafe { raw.data.mouse };
    let extent = if mouse.usFlags & MOUSE_VIRTUAL_DESKTOP != 0 {
        // SAFETY: by-value index constants.
        unsafe { (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)) }
    } else {
        // SAFETY: as above.
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
    };
    Some(raw_motion(mouse.usFlags, mouse.lLastX, mouse.lLastY, last_absolute, extent))
}

/// The signed wheel distance in the high word of a `WM_MOUSEWHEEL`/`WM_MOUSEHWHEEL`'s `WPARAM`.
///
/// **Signed**, and that is the whole of it: a notch towards the user is `-120`, which read as
/// unsigned is 65,416.
const fn wheel_delta(wparam: WPARAM) -> i32 {
    ((wparam >> 16) & 0xffff) as u16 as i16 as i32
}

/// `(dx, dy)` of a wheel message: `WM_MOUSEHWHEEL` is the horizontal axis, `WM_MOUSEWHEEL` the
/// vertical.
const fn wheel_motion(msg: u32, delta: i32) -> (i32, i32) {
    if msg == WM_MOUSEHWHEEL { (delta, 0) } else { (0, delta) }
}

/// Declare per-monitor DPI awareness, once per process, before any window exists.
///
/// Without it Win32 virtualises this process's coordinates on a scaled display: `GetClientRect`
/// reports a size in *logical* pixels and the compositor stretches whatever is rendered into it,
/// so a swapchain built from that number is both blurry and a different size from the one the
/// caller thinks it asked for. This seam's contract is physical pixels (see [`super`]), and this
/// call is what makes that true.
///
/// **Failure is ignored on purpose, and there is only one way it can fail.**
/// `SetProcessDpiAwarenessContext` returns `ERROR_ACCESS_DENIED` when the awareness has already
/// been set — by an application manifest, or by an embedder that called this first. In every such
/// case the process already has *an* awareness mode and cannot be given another, so there is
/// nothing to report and nothing to do: refusing to create a window because the embedder had
/// already made this decision would be worse than honouring their decision. What the caller gets
/// either way is what [`Window::client_size`] measures, which is the number that matters.
fn declare_dpi_awareness() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // SAFETY: takes one by-value context constant and touches no memory. It is documented as
        // callable only before the first window is created, which the `OnceLock` in
        // `window_class` guarantees by calling this from there.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    });
}

/// The class name, NUL-terminated UTF-16, with a lifetime that outlives every window.
///
/// A `Vec` in a `OnceLock` rather than a local: `RegisterClassW` is not documented to copy the
/// string it is given, and a dangling class name is the kind of bug that works until the
/// allocator reuses the page.
fn class_name() -> &'static [u16] {
    static NAME: OnceLock<Vec<u16>> = OnceLock::new();
    NAME.get_or_init(|| "Omnidroid.Window\0".encode_utf16().collect())
}

/// Register the window class, once per process, and return its atom.
///
/// The atom rather than the name: `CreateWindowExW` accepts either, an atom cannot collide with
/// another module's class of the same name, and passing it needs no string to stay alive.
///
/// Registration is per-process and the result is cached including its failure, so a second window
/// after a failed first one reports the same reason rather than trying again and getting
/// `ERROR_CLASS_ALREADY_EXISTS` from a half-registered state.
fn window_class() -> Result<(u16, HINSTANCE), u32> {
    static CLASS: OnceLock<Result<(u16, isize), u32>> = OnceLock::new();
    let cached = CLASS.get_or_init(|| {
        declare_dpi_awareness();
        // SAFETY: a null module name asks for the handle of the process's own executable image,
        // which always exists. The returned handle is a pseudo-handle for the loaded module and
        // must not be closed.
        let hinstance = unsafe { GetModuleHandleW(core::ptr::null()) };
        let class = WNDCLASSW {
            // No class styles. `CS_HREDRAW`/`CS_VREDRAW` force a repaint of a window whose
            // painting this process does not do — the swapchain owns every pixel of the client
            // area — and `CS_OWNDC` exists for OpenGL, which this is not.
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: core::ptr::null_mut(),
            // SAFETY: a null instance with a `IDC_*` integer resource asks for a system cursor,
            // which is a shared resource that is never unloaded. Without this the cursor keeps
            // whatever shape it had when it entered the client area — commonly the resize arrows
            // from the border it crossed.
            hCursor: unsafe { LoadCursorW(core::ptr::null_mut(), IDC_ARROW) },
            // **Null, so the client area is never erased.** A background brush would paint the
            // whole client area a flat colour before every paint, which is a full-window flash
            // behind a swapchain that is about to overwrite it anyway. The cost is that the
            // client area holds undefined pixels between creation and the first present, which
            // is why `Window::create` does not show the window.
            hbrBackground: core::ptr::null_mut(),
            lpszMenuName: core::ptr::null(),
            lpszClassName: class_name().as_ptr(),
        };
        // SAFETY: `class` is a fully-initialised `WNDCLASSW` living until this call returns, and
        // its two pointers outlive it — the cursor is a system resource and the class name is a
        // `'static`.
        let atom = unsafe { RegisterClassW(&raw const class) };
        if atom == 0 {
            // SAFETY: no arguments, no memory, and `RegisterClassW` is the last call this thread
            // made.
            return Err(unsafe { GetLastError() });
        }
        Ok((atom, hinstance as isize))
    });
    cached.map(|(atom, hinstance)| (atom, hinstance as HINSTANCE))
}

/// `LOWORD`/`HIWORD` of an `LPARAM` as a signed client coordinate.
///
/// The `as i16` is load-bearing and is the classic Win32 mistake: mouse coordinates are **signed**
/// 16-bit, and while the pointer is captured they are routinely negative — a drag off the left
/// edge reports `-1`, which read as unsigned is 65,535.
const fn mouse_xy(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam & 0xffff) as u16 as i16 as i32;
    let y = ((lparam >> 16) & 0xffff) as u16 as i16 as i32;
    (x, y)
}

/// The physical key a `WM_KEYDOWN`/`WM_KEYUP` names: the set-1 make code in bits 16-23 of its
/// `LPARAM`, with `0xE000` added when bit 24 -- the extended-key flag -- is set. See
/// [`WindowEvent::KeyDown`]'s `scancode`.
///
/// **The flag is not optional.** Left and right Ctrl share make code `0x1D`, and the arrow keys
/// share theirs with the numeric keypad's 8/4/6/2; only bit 24 tells them apart.
const fn scancode_of(lparam: LPARAM) -> u32 {
    let make = ((lparam >> 16) & 0xff) as u32;
    if (lparam >> 24) & 1 != 0 { 0xE000 | make } else { make }
}

/// What one `WM_CHAR` is: the text it completes, if any. See [`WindowEvent::Text`].
///
/// `unit` is the message's `WPARAM`, which for this window is one UTF-16 code unit; `pending` is
/// the window's [`WindowState::high_surrogate`].
///
/// * A high surrogate is held in `pending` and produces nothing yet.
/// * A low surrogate completes a held high one into one character. With none held it is dropped.
/// * Anything else is a character on its own, and it discards a held high surrogate, whose
///   partner is now never coming: half a pair is not a character and has no UTF-8 spelling. A
///   second high surrogate likewise replaces the first.
/// * A C0 control code or DEL is then dropped; see [`WindowEvent::Text`] for why those are keys
///   rather than text.
///
/// Total over `u16`: no unit panics, and anything returned is one valid, non-control character.
fn text_from_char(pending: &mut Option<u16>, unit: u16) -> Option<String> {
    let character = match (pending.take(), unit) {
        (_, 0xD800..=0xDBFF) => {
            *pending = Some(unit);
            return None;
        }
        (Some(high), 0xDC00..=0xDFFF) => char::decode_utf16([high, unit]).next()?.ok()?,
        // A lone low surrogate is not a Unicode scalar value, so `from_u32` refuses it.
        _ => char::from_u32(u32::from(unit))?,
    };
    let control = character < ' ' || character == '\u{7F}';
    (!control).then(|| character.to_string())
}

/// The window procedure.
///
/// Every arm is a translation into a [`WindowEvent`]; nothing here decides anything. Messages this
/// function does not name fall through to `DefWindowProcW`, which is what makes the window behave
/// like a window — moving, resizing, the system menu, `Alt+F4`.
///
/// # Safety
///
/// Called by Win32 with a valid `hwnd` on the thread that created it. The `GWLP_USERDATA` slot is
/// either null (before `WM_NCCREATE` and after `WM_NCDESTROY`) or a pointer to a live
/// [`WindowState`] owned by the [`Window`] that created this `hwnd`.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        // SAFETY: for `WM_NCCREATE`, Win32 documents `lparam` as a pointer to a `CREATESTRUCTW`
        // that is live for the duration of this call, and `lpCreateParams` as the pointer this
        // process passed to `CreateWindowExW`.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        // SAFETY: setting `GWLP_USERDATA` on a window of a class with `cbWndExtra == 0` stores
        // the value in the per-window slot Win32 reserves for exactly this purpose.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
        // Fall through: `DefWindowProcW` must see `WM_NCCREATE` or the window is not created.
        // SAFETY: forwarding the arguments unchanged.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    // SAFETY: reading back the slot written above. Null before `WM_NCCREATE`, which `DestroyWindow`
    // and the pre-creation messages both produce.
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState;
    if state.is_null() {
        // SAFETY: forwarding the arguments unchanged.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: the slot is non-null, so `Window::create` put a live `WindowState` there and
    // `Window::drop` has not yet run — it clears the slot from `WM_NCDESTROY` below, which is the
    // last message this window ever receives. Win32 delivers messages for a window only on the
    // thread that created it, and `Window` is `!Send`, so no other thread holds a reference.
    let state = unsafe { &mut *state };

    match msg {
        WM_NCDESTROY => {
            // The last message. Clear the slot so that anything Win32 sends afterwards — and it
            // does not, but "does not" is not a guarantee this file can make — finds null rather
            // than a pointer whose allocation `Window::drop` is about to reclaim.
            // SAFETY: as the write in the `WM_NCCREATE` arm.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
        }
        WM_SIZE => {
            // `WM_SIZE`'s `LPARAM` is the new *client* size, which is the size a swapchain wants,
            // as two unsigned 16-bit halves. A minimised window reports `(0, 0)`.
            let width = u32::from((lparam & 0xffff) as u16);
            let height = u32::from(((lparam >> 16) & 0xffff) as u16);
            if state.last_size != (width, height) {
                state.last_size = (width, height);
                push_event(&mut state.queue, WindowEvent::Resized { width, height });
            }
            // A presented image is stretched to the new size everywhere, not only over the strip
            // the resize exposed (point 8).
            if lock(&state.canvas).image.is_some() {
                // SAFETY: a live window handle; a null rectangle is the whole client area.
                unsafe { InvalidateRect(hwnd, core::ptr::null(), 0) };
            }
        }
        WM_PAINT => {
            if paint(hwnd, state) {
                return 0;
            }
        }
        // **The display under the window changed** (`WindowEvent::DisplayChanged`). All three
        // still go on to `DefWindowProcW`: `WM_WINDOWPOSCHANGED`'s default is what sends `WM_SIZE`
        // and `WM_MOVE`, and `WM_DPICHANGED`'s suggested rectangle is not acted on here -- a
        // window of this process keeps its physical size across a scale change, which is what the
        // seam's pixels promise.
        WM_DISPLAYCHANGE => {
            push_event(&mut state.queue, WindowEvent::DisplayChanged { change: DisplayChange::Mode });
        }
        WM_DPICHANGED => {
            push_event(&mut state.queue, WindowEvent::DisplayChanged { change: DisplayChange::Scale });
        }
        WM_WINDOWPOSCHANGED => {
            // SAFETY: a live window handle; the answer is a handle, never null with this flag.
            let now = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
            if !state.monitor.is_null() && now != state.monitor {
                push_event(&mut state.queue, WindowEvent::DisplayChanged { change: DisplayChange::Monitor });
            }
            state.monitor = now;
        }
        WM_CLOSE => {
            // Swallowed; see this module's point 2.
            push_event(&mut state.queue, WindowEvent::CloseRequested);
            return 0;
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            // A capture ends with the focus (this module's point 5), and is reported before the
            // focus change, so a consumer handling the focus loss already knows the pointer is
            // free.
            let capture_ended = msg == WM_KILLFOCUS && state.captured.is_some();
            if capture_ended {
                end_capture(state);
                push_event(&mut state.queue, WindowEvent::PointerCaptureLost);
            }
            state.focused = msg == WM_SETFOCUS;
            // A cursor the capture hid is shown the moment the focus goes, without waiting for a
            // move (point 6) -- unless the pointer is over a client area asked to hide it.
            if capture_ended {
                refresh_cursor(hwnd, state);
            }
            push_event(&mut state.queue, WindowEvent::FocusChanged {
                focused: msg == WM_SETFOCUS,
            });
        }
        WM_MOUSEMOVE => {
            // Not while captured: the cursor is pinned, and what moves is `WM_INPUT`'s.
            if state.captured.is_none() {
                let (x, y) = mouse_xy(lparam);
                state.last_pointer = (x, y);
                push_event(&mut state.queue, WindowEvent::PointerMoved { x, y });
            }
        }
        WM_SETCURSOR => {
            // Hidden over the client area while captured, or while asked to be, focused or not
            // (point 6); the class's arrow everywhere else, and over the client area otherwise,
            // from `DefWindowProcW`.
            if state.cursor_invisible() && (lparam & 0xffff) as u32 == HTCLIENT {
                // SAFETY: a null cursor hides it; no memory is read.
                unsafe { SetCursor(core::ptr::null_mut()) };
                // `TRUE`: handled, so `DefWindowProcW` does not set the arrow back.
                return 1;
            }
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = wheel_delta(wparam);
            // The position is in **screen** coordinates for these two, unlike every other mouse
            // message, and signed for the same reason as `mouse_xy`'s.
            let (x, y) = mouse_xy(lparam);
            let mut at = POINT { x, y };
            // SAFETY: a live window handle and a `POINT` written in place.
            unsafe { ScreenToClient(hwnd, &raw mut at) };
            let (dx, dy) = wheel_motion(msg, delta);
            push_event(&mut state.queue, WindowEvent::Wheel { x: at.x, y: at.y, dx, dy });
            // Processed, which both messages' contract says to report with 0.
            return 0;
        }
        WM_INPUT => {
            // Only while captured; otherwise raw input is not registered and this does not arrive
            // -- except for one already queued when a capture ended, which is dropped here.
            if state.captured.is_some() {
                if let Some((dx, dy)) = read_raw_motion(lparam, &mut state.last_absolute) {
                    if (dx, dy) != (0, 0) {
                        push_event(&mut state.queue, WindowEvent::PointerMotion { dx, dy });
                    }
                }
            }
            // Falls through: `DefWindowProcW` must see a `WM_INPUT` to release its data.
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
            let button = button_of(msg, wparam);
            let (x, y) = mouse_xy(lparam);
            state.last_pointer = (x, y);
            if state.buttons_down == 0 {
                // SAFETY: capturing to a window this thread owns; released below.
                unsafe { SetCapture(hwnd) };
            }
            state.buttons_down |= 1 << button_bit(button);
            push_event(&mut state.queue, WindowEvent::PointerDown { button, x, y });
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => {
            let button = button_of(msg, wparam);
            let (x, y) = mouse_xy(lparam);
            state.last_pointer = (x, y);
            state.buttons_down &= !(1 << button_bit(button));
            if state.buttons_down == 0 {
                // SAFETY: no arguments; a no-op when this thread holds no capture.
                unsafe { ReleaseCapture() };
            }
            push_event(&mut state.queue, WindowEvent::PointerUp { button, x, y });
        }
        WM_CAPTURECHANGED => {
            // Something else took the capture — a system drag, a menu, another window. The
            // releases will go there, so every button reported down is reported up here, now
            // (point 7); the mask would otherwise be a lie that never clears. This window's own
            // `ReleaseCapture` after the last release finds the mask empty and reports nothing.
            if lparam as HWND != hwnd {
                state.release_all_buttons();
            }
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            push_event(&mut state.queue, WindowEvent::KeyDown {
                keycode: wparam as u32,
                scancode: scancode_of(lparam),
                // Bit 30 of the `LPARAM` is the previous key state: set means the key was already
                // down, i.e. this is an auto-repeat.
                repeat: (lparam >> 30) & 1 != 0,
            });
        }
        WM_KEYUP | WM_SYSKEYUP => {
            push_event(&mut state.queue, WindowEvent::KeyUp {
                keycode: wparam as u32,
                scancode: scancode_of(lparam),
            });
        }
        WM_CHAR => {
            // One UTF-16 code unit in the low 16 bits of the `WPARAM`: the class is registered
            // with `RegisterClassW` and pumped with `PeekMessageW`/`DispatchMessageW`, so Win32
            // hands this window the Unicode spelling of the character. The repeat count in the
            // `LPARAM` is ignored, as `WM_KEYDOWN`'s is: one message, at most one character.
            if let Some(text) = text_from_char(&mut state.high_surrogate, wparam as u16) {
                push_event(&mut state.queue, WindowEvent::Text { text });
            }
            // Processed, which `WM_CHAR`'s contract says to report with 0.
            return 0;
        }
        _ => {}
    }

    // Everything above except `WM_CLOSE` and `WM_CHAR` still wants the default behaviour:
    // `WM_SIZE` and the focus messages have real work behind them, and swallowing the key
    // messages would break `Alt+F4` and the system menu. (`WM_SYSCHAR`, a character typed with
    // Alt held, is not text and is not named above, so it reaches `DefWindowProcW` as before.)
    // SAFETY: forwarding the arguments unchanged.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// `WM_PAINT` for a window with a presented image: the image stretched over the whole client area
/// (point 8). `false`, having done nothing, for a window with none -- its `WM_PAINT` is
/// `DefWindowProcW`'s. `BeginPaint` validates the update region whether or not anything is drawn,
/// so a paint that cannot draw still ends the `WM_PAINT`s for it.
fn paint(hwnd: HWND, state: &WindowState) -> bool {
    let canvas = lock(&state.canvas);
    let Some(image) = &canvas.image else { return false };
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: a live window handle, in its `WM_PAINT`; writes the `PAINTSTRUCT`.
    let hdc = unsafe { BeginPaint(hwnd, &raw mut ps) };
    if !hdc.is_null() {
        let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: writes a `RECT`; the handle is live.
        unsafe { GetClientRect(hwnd, &raw mut client) };
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                // Both bounded to `MAX_EXTENT` by `super::validate_extent`.
                biWidth: image.width as i32,
                biHeight: -(image.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: the DC `BeginPaint` gave; `bgra` holds `width * height` 4-byte pixels, which is
        // what the header describes, and lives across the call.
        unsafe {
            // Nearest-pixel stretching: the default (`BLACKONWHITE`) ANDs the rows it drops.
            SetStretchBltMode(hdc, COLORONCOLOR);
            StretchDIBits(
                hdc,
                0,
                0,
                client.right,
                client.bottom,
                0,
                0,
                image.width as i32,
                image.height as i32,
                image.bgra.as_ptr().cast(),
                &raw const info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
    // SAFETY: pairs the `BeginPaint` above, with its `PAINTSTRUCT`.
    unsafe { EndPaint(hwnd, &raw const ps) };
    true
}

/// Which button a mouse message is about.
///
/// The `WM_XBUTTON*` pair does not say in the message id — both extended buttons share it and the
/// high word of the `WPARAM` distinguishes them.
fn button_of(msg: u32, wparam: WPARAM) -> PointerButton {
    match msg {
        WM_LBUTTONDOWN | WM_LBUTTONUP => PointerButton::Primary,
        WM_RBUTTONDOWN | WM_RBUTTONUP => PointerButton::Secondary,
        WM_MBUTTONDOWN | WM_MBUTTONUP => PointerButton::Middle,
        _ => {
            if ((wparam >> 16) & 0xffff) as u16 == XBUTTON1 {
                PointerButton::Back
            } else {
                PointerButton::Forward
            }
        }
    }
}

/// The bit [`WindowState::buttons_down`] tracks this button in.
const fn button_bit(button: PointerButton) -> u32 {
    match button {
        PointerButton::Primary => 0,
        PointerButton::Secondary => 1,
        PointerButton::Middle => 2,
        PointerButton::Back => 3,
        PointerButton::Forward => 4,
    }
}

/// A Win32 window.
pub(super) struct Window {
    hwnd: HWND,
    hinstance: HINSTANCE,
    /// Owned. Reclaimed in [`Window::drop`] after `DestroyWindow` has returned, so that the
    /// `WM_DESTROY`/`WM_NCDESTROY` the window procedure receives from inside that call still find
    /// it live. Held as a raw pointer rather than a `Box` for the aliasing reason
    /// [`WindowState`] records.
    state: *mut WindowState,
}

impl Window {
    /// `CreateWindowExW` with `WS_OVERLAPPEDWINDOW`, then correct the size to what was asked for.
    ///
    /// `WS_OVERLAPPEDWINDOW` is the resizable style: it is `WS_OVERLAPPED | WS_CAPTION |
    /// WS_SYSMENU | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX`, and `WS_THICKFRAME` is the
    /// draggable border the whole swapchain-recreation path exists to survive.
    ///
    /// # The size is measured rather than predicted, and that is the interesting part
    ///
    /// `CreateWindowExW` is given an *outer* size and the caller asked for a *client* size, so
    /// something has to account for the frame and title bar. The obvious call is
    /// `AdjustWindowRectEx` — and it is wrong here, because it computes the frame for the
    /// **system** DPI while this process is per-monitor aware, so on a 150%-scaled display it
    /// under-reports the frame and the client area comes out short. The DPI-aware spelling,
    /// `AdjustWindowRectExForDpi`, needs the DPI of the monitor the window will land on, which is
    /// not knowable before it has landed.
    ///
    /// So the window is created with the requested client size as its outer size, its *actual*
    /// client area is measured with `GetClientRect`, and the outer size is corrected once by the
    /// difference. That is one code path with no prediction in it, correct at any scale factor
    /// and on any future frame metric — and, because it is the only path, it is exercised by every
    /// window this backend ever creates rather than by a branch that fires on machines nobody
    /// tests on (VERIFICATION entry 12).
    ///
    /// **The OS may still refuse the size**, and the caller must expect that: Windows enforces a
    /// minimum tracking size of roughly 130 physical pixels of width, so a narrower request comes
    /// back wider. [`Window::client_size`] reports what actually happened, which is why this seam
    /// never caches the size it asked for.
    pub(super) fn create(desc: &WindowDesc<'_>) -> WindowResult<Self> {
        let (atom, hinstance) = window_class().map_err(|code| WindowError::LastError {
            operation: "create",
            api: "RegisterClassW",
            code,
        })?;

        let title: Vec<u16> = desc.title.encode_utf16().chain(core::iter::once(0)).collect();
        let state = Box::into_raw(Box::new(WindowState {
            queue: Vec::new(),
            last_size: (u32::MAX, u32::MAX),
            buttons_down: 0,
            high_surrogate: None,
            captured: None,
            last_absolute: None,
            monitor: core::ptr::null_mut(),
            hide_cursor: false,
            // `WM_SETFOCUS` says when it arrives: a window is created without the focus.
            focused: false,
            last_pointer: (0, 0),
            canvas: Arc::new(Mutex::new(Canvas { image: None, alive: true })),
        }));

        // `super::validate` has already bounded both axes to 1..=65535, so neither cast can
        // truncate or go negative.
        let want = (desc.width as i32, desc.height as i32);
        // SAFETY: the class atom is registered and lives for the process; `title` is a live
        // NUL-terminated UTF-16 buffer that outlives the call; `state` is the pointer the window
        // procedure will install from `CREATESTRUCTW::lpCreateParams`, and it is live because
        // nothing below frees it except on the error path, after this call has returned.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                atom as usize as *const u16,
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                want.0,
                want.1,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                hinstance,
                state.cast(),
            )
        };
        if hwnd.is_null() {
            // SAFETY: no arguments, and `CreateWindowExW` is the last call this thread made.
            let code = unsafe { GetLastError() };
            // SAFETY: `state` came from `Box::into_raw` moments ago and no window owns it — the
            // creation that would have taken it failed.
            drop(unsafe { Box::from_raw(state) });
            return Err(WindowError::LastError {
                operation: "create",
                api: "CreateWindowExW",
                code,
            });
        }

        let window = Window { hwnd, hinstance, state };
        window.set_client_size(desc.width, desc.height, "create")?;
        Ok(window)
    }

    /// Resize the *outer* window by however much the client area differs from `(width, height)`.
    ///
    /// See [`Window::create`] for why this is a measurement rather than a calculation, and
    /// [`super::Window::set_client_size`] for why the operation is public rather than a private
    /// step of creation. `operation` names the caller for the error message, because "create" and
    /// "set_client_size" fail here identically and a reader needs to know which one was running.
    ///
    /// A window whose client area already matches is left alone — not as an optimisation but
    /// because `SetWindowPos` would send a `WM_SIZE` naming a size that has not changed, which the
    /// window procedure would then have to filter back out.
    pub(super) fn set_client_size(
        &self,
        width: u32,
        height: u32,
        operation: &'static str,
    ) -> WindowResult<()> {
        let (have_w, have_h) = self.client_size()?;
        if (have_w, have_h) == (width, height) {
            return Ok(());
        }
        let mut outer = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: writes a `RECT` at the pointer and reads only the window handle, which is live.
        if unsafe { GetWindowRect(self.hwnd, &raw mut outer) } == 0 {
            // SAFETY: no arguments, and `GetWindowRect` is the last call this thread made.
            let code = unsafe { GetLastError() };
            return Err(WindowError::LastError {
                operation,
                api: "GetWindowRect",
                code,
            });
        }
        // The frame is the difference between the outer and client extents, so adding it to the
        // requested client size gives the outer size that produces it. `saturating_*` because
        // every term is OS-supplied and the arithmetic must not wrap on a hostile or degenerate
        // rectangle (VERIFICATION entry 3: a wrapped value can satisfy the assertion).
        let frame_w = (outer.right - outer.left).saturating_sub_unsigned(have_w);
        let frame_h = (outer.bottom - outer.top).saturating_sub_unsigned(have_h);
        let outer_w = frame_w.saturating_add_unsigned(width);
        let outer_h = frame_h.saturating_add_unsigned(height);
        // SAFETY: a live window handle, a null insert-after handle made irrelevant by
        // `SWP_NOZORDER`, and two sizes. `SWP_NOMOVE` means the x/y arguments are ignored.
        if unsafe {
            SetWindowPos(
                self.hwnd,
                core::ptr::null_mut(),
                0,
                0,
                outer_w,
                outer_h,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        } == 0
        {
            // SAFETY: no arguments, and `SetWindowPos` is the last call this thread made.
            let code = unsafe { GetLastError() };
            return Err(WindowError::LastError {
                operation,
                api: "SetWindowPos",
                code,
            });
        }
        Ok(())
    }

    /// `ShowWindow(SW_SHOWNORMAL)`: visible, unminimised, and activated.
    pub(super) fn show(&self) {
        // SAFETY: a live window handle and a command constant. The return value is the window's
        // *previous* visibility, not a success code, so there is nothing to check.
        unsafe { ShowWindow(self.hwnd, SW_SHOWNORMAL) };
    }

    /// Pump this window's messages and move everything the window procedure queued into `sink`.
    ///
    /// `PeekMessageW` with `PM_REMOVE` rather than `GetMessageW`: it returns 0 immediately when
    /// the queue is empty, which is the seam's central promise.
    ///
    /// **The `hwnd` filter is deliberate.** One thread may own several windows — D10's
    /// multi-instance case in a single process — and an unfiltered pump would let whichever
    /// window happened to be polled first dispatch, and account for, the others' messages.
    /// Dispatching still reaches the right window procedure either way, so the bug would not be
    /// visible as a lost event; it would be visible as a window that only produces events when a
    /// different window is polled.
    pub(super) fn poll(&mut self, sink: &mut Vec<WindowEvent>) {
        let mut msg = MSG::default();
        loop {
            // SAFETY: writes a `MSG` at the pointer; the handle filters to this live window.
            let got = unsafe { PeekMessageW(&raw mut msg, self.hwnd, 0, 0, PM_REMOVE) };
            if got == 0 {
                break;
            }
            // SAFETY: `msg` was just filled by `PeekMessageW`. `TranslateMessage` posts `WM_CHAR`
            // for a key message that types something, through the thread's keyboard layout, and
            // that `WM_CHAR` is what becomes a `WindowEvent::Text` — without this call typed text
            // is silently impossible. It is retrieved by a later iteration of this loop, after
            // the key message it came from has been dispatched, which is what puts the `Text`
            // behind its `KeyDown`. `DispatchMessageW` re-enters `wnd_proc`, which is why no
            // reference into `*self.state` is held across this call.
            unsafe {
                TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        // SAFETY: `self.state` is live for as long as `self` is, and the pump above has returned,
        // so the window procedure is not running and holds no reference into it.
        let state = unsafe { &mut *self.state };
        // A clip the system lifted under a held capture is put back (point 5). Only while this
        // window has the focus: without it the capture has already ended, from `WM_KILLFOCUS`.
        // SAFETY: no arguments.
        if let (Some(at), true) = (state.captured, unsafe { GetFocus() } == self.hwnd) {
            let _ = clip_to(at);
        }
        // A button reported down that the host says is up is released (point 7). The queue was
        // drained first, so a release this window was sent is already in it.
        if state.buttons_down != 0 {
            // SAFETY: by-value index constant.
            let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
            let (x, y) = state.last_pointer;
            for button in ALL_BUTTONS {
                let bit = 1 << button_bit(button);
                // SAFETY: a virtual-key code. The high bit is "down now".
                let down = unsafe { GetAsyncKeyState(i32::from(physical_key(button, swapped))) } as u16 & 0x8000 != 0;
                if state.buttons_down & bit != 0 && !down {
                    state.buttons_down &= !bit;
                    push_event(&mut state.queue, WindowEvent::PointerUp { button, x, y });
                }
            }
            if state.buttons_down == 0 {
                // SAFETY: no arguments; a no-op when this thread holds no capture. Sends
                // `WM_CAPTURECHANGED` naming no window, which finds the mask empty.
                unsafe { ReleaseCapture() };
            }
        }
        sink.append(&mut state.queue);
    }

    /// See [`super::Window::set_pointer_capture`] and this module's point 5.
    pub(super) fn set_pointer_capture(&mut self, captured: bool) -> WindowResult<bool> {
        // SAFETY: as in `poll`: live for as long as `self`, and the window procedure is not
        // running -- nothing below sends this window a message.
        let state = unsafe { &mut *self.state };
        if !captured {
            if let Some(held) = state.captured {
                end_capture(state);
                // Visible again at once, where it was held, rather than at the next move -- unless
                // it is to stay hidden there (point 6).
                refresh_cursor(self.hwnd, state);
                // And where that is, said: the consumer followed the motion, not the cursor.
                let mut at = held;
                // SAFETY: a live window handle and a `POINT` written in place.
                unsafe { ScreenToClient(self.hwnd, &raw mut at) };
                push_event(&mut state.queue, WindowEvent::PointerMoved { x: at.x, y: at.y });
            }
            return Ok(false);
        }
        if state.captured.is_some() {
            return Ok(true);
        }
        // SAFETY: no arguments.
        if unsafe { GetFocus() } != self.hwnd {
            return Ok(false);
        }
        let failed = |api: &'static str, code: u32| WindowError::LastError {
            operation: "set_pointer_capture",
            api,
            code,
        };
        // **Where the cursor is held: where it is, inside the client area.** A request can come
        // while the cursor is outside it -- a button held since a press inside, with `SetCapture`
        // still reporting -- and a cursor held outside the window would be hidden nowhere.
        let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: writes a `RECT`; the handle is live.
        if unsafe { GetClientRect(self.hwnd, &raw mut client) } == 0 || client.right <= 0 || client.bottom <= 0 {
            // A minimised window has no client area to hold the cursor in.
            return Ok(false);
        }
        let mut origin = POINT { x: 0, y: 0 };
        // SAFETY: a live window handle and a `POINT` written in place.
        unsafe { ClientToScreen(self.hwnd, &raw mut origin) };
        let mut at = POINT { x: 0, y: 0 };
        // SAFETY: writes a `POINT`.
        if unsafe { GetCursorPos(&raw mut at) } == 0 {
            // SAFETY: no arguments, and `GetCursorPos` is the last call this thread made.
            return Err(failed("GetCursorPos", unsafe { GetLastError() }));
        }
        let held = POINT {
            x: at.x.clamp(origin.x, origin.x + client.right - 1),
            y: at.y.clamp(origin.y, origin.y + client.bottom - 1),
        };
        if (held.x, held.y) != (at.x, at.y) {
            // SAFETY: by-value coordinates.
            unsafe { SetCursorPos(held.x, held.y) };
        }
        register_raw_mouse(self.hwnd, true).map_err(|code| failed("RegisterRawInputDevices", code))?;
        if let Err(code) = clip_to(held) {
            let _ = register_raw_mouse(self.hwnd, false);
            return Err(failed("ClipCursor", code));
        }
        state.captured = Some(held);
        state.last_absolute = None;
        // Hidden at once; `WM_SETCURSOR` keeps it hidden.
        // SAFETY: a null cursor hides it.
        unsafe { SetCursor(core::ptr::null_mut()) };
        Ok(true)
    }

    /// Whether the capture is held.
    pub(super) fn has_pointer_capture(&self) -> bool {
        // SAFETY: live for as long as `self`; a read.
        unsafe { (*self.state).captured.is_some() }
    }

    /// See [`super::Window::set_cursor_hidden`] and this module's point 6.
    pub(super) fn set_cursor_hidden(&mut self, hidden: bool) -> WindowResult<()> {
        // SAFETY: as in `set_pointer_capture`.
        let state = unsafe { &mut *self.state };
        if state.hide_cursor != hidden {
            state.hide_cursor = hidden;
            refresh_cursor(self.hwnd, state);
        }
        Ok(())
    }

    /// The request (point 6: not scoped to the focus).
    pub(super) fn cursor_hidden(&self) -> bool {
        // SAFETY: live for as long as `self`; a read.
        unsafe { (*self.state).hide_cursor }
    }

    /// See [`super::Window::warp_pointer`]. While captured the held pixel moves with it: the clip
    /// is moved first, because `SetCursorPos` is confined by it.
    pub(super) fn warp_pointer(&mut self, x: i32, y: i32) -> WindowResult<()> {
        // SAFETY: as in `set_pointer_capture`.
        let state = unsafe { &mut *self.state };
        let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: writes a `RECT`; the handle is live.
        if unsafe { GetClientRect(self.hwnd, &raw mut client) } == 0 || client.right <= 0 || client.bottom <= 0 {
            // Minimised: there is nowhere in it to put the cursor.
            return Ok(());
        }
        let (x, y) = (x.clamp(0, client.right - 1), y.clamp(0, client.bottom - 1));
        let mut at = POINT { x, y };
        // SAFETY: a live window handle and a `POINT` written in place.
        unsafe { ClientToScreen(self.hwnd, &raw mut at) };
        if state.captured.is_some() {
            clip_to(at).map_err(|code| WindowError::LastError { operation: "warp_pointer", api: "ClipCursor", code })?;
            state.captured = Some(at);
        }
        // SAFETY: by-value coordinates.
        if unsafe { SetCursorPos(at.x, at.y) } == 0 {
            // SAFETY: no arguments, and `SetCursorPos` is the last call this thread made.
            let code = unsafe { GetLastError() };
            return Err(WindowError::LastError { operation: "warp_pointer", api: "SetCursorPos", code });
        }
        state.last_pointer = (x, y);
        Ok(())
    }

    /// From `WM_SETFOCUS`/`WM_KILLFOCUS`.
    pub(super) fn has_focus(&self) -> bool {
        // SAFETY: live for as long as `self`; a read.
        unsafe { (*self.state).focused }
    }

    /// `MsgWaitForMultipleObjectsEx` on no handles: wake for any input or message for this thread.
    ///
    /// **`MWMO_INPUTAVAILABLE` is load-bearing.** Without it the wait wakes only for input that
    /// arrived *since the last* `PeekMessageW`, so a message a previous pump looked at and left
    /// would put the thread to sleep on a non-empty queue for the whole timeout.
    pub(super) fn wait(&self, timeout: Duration) -> bool {
        // SAFETY: live for as long as `self`; a read.
        if !unsafe { &*self.state }.queue.is_empty() {
            return true;
        }
        // `INFINITE` is `u32::MAX`; a finite request never becomes it.
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1).min(u32::MAX - 1);
        // SAFETY: no handles, so a null array with a count of zero.
        let woke = unsafe {
            MsgWaitForMultipleObjectsEx(0, core::ptr::null(), millis, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
        };
        if woke == WAIT_FAILED {
            // Documented only for bad handles, of which there are none. A spin would be the cost
            // of trusting that, so a failed wait still waits.
            std::thread::sleep(timeout);
            return false;
        }
        woke == WAIT_OBJECT_0
    }

    /// `GetClientRect`, which reports the drawable area in physical pixels.
    ///
    /// The rectangle's origin is always `(0, 0)`, so `right`/`bottom` *are* the extent. Both are
    /// non-negative for any window, minimised included, so the casts cannot go wrong; they are
    /// done with `unsigned_abs` rather than `as u32` so that an impossible negative from a future
    /// Win32 would come out as a large number a test can catch rather than as a wrapped one.
    pub(super) fn client_size(&self) -> WindowResult<(u32, u32)> {
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: writes a `RECT` at the pointer and reads only the window handle, which is live.
        if unsafe { GetClientRect(self.hwnd, &raw mut rect) } == 0 {
            // SAFETY: no arguments, and `GetClientRect` is the last call this thread made.
            let code = unsafe { GetLastError() };
            return Err(WindowError::LastError {
                operation: "client_size",
                api: "GetClientRect",
                code,
            });
        }
        Ok((
            (rect.right - rect.left).unsigned_abs(),
            (rect.bottom - rect.top).unsigned_abs(),
        ))
    }

    /// `GetDpiForWindow`: the window's DPI under this process's per-monitor awareness.
    pub(super) fn dpi(&self) -> WindowResult<u32> {
        // SAFETY: reads only the window handle, which is live.
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        if dpi == 0 {
            // SAFETY: no arguments, and `GetDpiForWindow` is the last call this thread made.
            let code = unsafe { GetLastError() };
            return Err(WindowError::LastError { operation: "dpi", api: "GetDpiForWindow", code });
        }
        Ok(dpi)
    }

    /// `ShowWindow(SW_MINIMIZE)` or `ShowWindow(SW_RESTORE)`.
    ///
    /// `SW_RESTORE` rather than `SW_SHOWNORMAL` for the restore: it returns a maximised window to
    /// *maximised* and a normal one to normal, where `SW_SHOWNORMAL` would silently un-maximise a
    /// window the user had maximised before minimising it.
    ///
    /// Infallible on Windows — `ShowWindow` returns the previous visibility, not a success code —
    /// so the `Result` is the seam's shape rather than this backend's need for one.
    pub(super) fn set_minimized(&self, minimized: bool) -> WindowResult<()> {
        let command = if minimized { SW_MINIMIZE } else { SW_RESTORE };
        // SAFETY: a live window handle and a command constant.
        unsafe { ShowWindow(self.hwnd, command) };
        Ok(())
    }

    /// `PostMessageW(WM_CLOSE)` — the same message the title-bar button sends.
    ///
    /// Posted rather than sent, so that it arrives through the queue like a user's click and is
    /// seen by the next [`Window::poll`]. Sending it would run the window procedure on this
    /// thread immediately, which works and is a second code path for something that already has
    /// one.
    pub(super) fn request_close(&self) -> WindowResult<()> {
        // SAFETY: a live window handle and a message with no pointer arguments.
        if unsafe { PostMessageW(self.hwnd, WM_CLOSE, 0, 0) } == 0 {
            // SAFETY: no arguments, and `PostMessageW` is the last call this thread made.
            let code = unsafe { GetLastError() };
            return Err(WindowError::LastError {
                operation: "request_close",
                api: "PostMessageW",
                code,
            });
        }
        Ok(())
    }

    /// Keep the image (as BGRA) and paint it now: point 8.
    pub(super) fn present_rgba(&mut self, rgba: &[u8], width: u32, height: u32) -> WindowResult<()> {
        self.presenter().present_rgba(rgba, width, height)?;
        // SAFETY: a live window handle. `UpdateWindow` sends the `WM_PAINT` the presenter's
        // invalidation asked for to `wnd_proc` on this thread before it returns (none for a
        // minimised window, which paints when restored); no reference into the state is held.
        unsafe { UpdateWindow(self.hwnd) };
        Ok(())
    }

    /// A handle presenting to this window from any thread (point 8).
    pub(super) fn presenter(&self) -> Presenter {
        // SAFETY: live for as long as `self`; the `Arc` is cloned, nothing else is touched.
        let canvas = Arc::clone(&unsafe { &*self.state }.canvas);
        Presenter { hwnd: self.hwnd as isize, canvas }
    }

    /// The `HWND` and the `HINSTANCE` its class was registered with.
    ///
    /// Both, because `VkWin32SurfaceCreateInfoKHR` has a field for each.
    pub(super) fn raw(&self) -> RawWindow {
        RawWindow::Win32 { hwnd: self.hwnd as isize, hinstance: self.hinstance as isize }
    }
}

impl Drop for Window {
    /// `DestroyWindow` **first**, then reclaim the state.
    ///
    /// The order is the whole of it. `DestroyWindow` synchronously sends `WM_DESTROY` and
    /// `WM_NCDESTROY` to the window procedure on this thread, and both look the state pointer up
    /// out of `GWLP_USERDATA`; freeing the allocation first would hand them a dangling pointer
    /// from inside `Drop`, which is a use-after-free with no failure mode anyone would debug in
    /// under an hour.
    fn drop(&mut self) {
        // A held capture first: the clip is process-wide and would outlive the window, pinning the
        // cursor to a pixel nothing owns (point 5).
        // SAFETY: live until reclaimed below; the window procedure is not running.
        let state = unsafe { &mut *self.state };
        let invisible = state.cursor_invisible();
        end_capture(state);
        // And a cursor this window hid is shown, where it is, before the window goes.
        state.hide_cursor = false;
        if invisible {
            refresh_cursor(self.hwnd, state);
        }
        // Presenters stop before the handle can go stale (point 8).
        lock(&state.canvas).alive = false;
        // SAFETY: a live window handle, destroyed from the thread that created it — which is the
        // only thread that can hold a `Window`, because it is `!Send`.
        unsafe { DestroyWindow(self.hwnd) };
        // SAFETY: `state` came from `Box::into_raw` in `create` and nothing else owns it. The
        // window is gone, so `wnd_proc` can no longer be entered for it.
        drop(unsafe { Box::from_raw(self.state) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `W` pressed, as Win32 packs it: a repeat count of 1 in bits 0-15, make code `0x11` in bits
    /// 16-23; right Ctrl with the extended flag in bit 24; and a key-up's transition bits 30-31,
    /// which must not leak into the code.
    #[test]
    fn the_scancode_is_the_make_code_and_the_extended_flag() {
        assert_eq!(scancode_of(0x0011_0001), 0x11);
        assert_eq!(scancode_of(0x011D_0001), 0xE01D);
        assert_eq!(scancode_of(0x001D_0001), 0x1D, "left Ctrl is not right Ctrl");
        assert_eq!(scancode_of(0x0148_0001), 0xE048, "the Up arrow, not keypad 8");
        assert_eq!(scancode_of(0xC011_0001_u32 as i32 as LPARAM), 0x11, "a key-up's high bits");
    }

    /// A notch away from the user is `+120` in the high word, towards is `-120` -- which, read
    /// unsigned, would be 65,416 -- and the low word (the button and modifier state) is not part of
    /// it.
    #[test]
    fn the_wheel_delta_is_the_signed_high_word() {
        assert_eq!(wheel_delta(0x0078_0000), 120);
        assert_eq!(wheel_delta(0xFF88_0000), -120, "a notch towards the user");
        assert_eq!(wheel_delta(0xFF88_0008), -120, "MK_CONTROL in the low word is not the delta");
        assert_eq!(wheel_delta(0x0001_0000), 1, "a high-resolution wheel's fraction of a notch");
        assert_eq!(wheel_delta(0x8000_0000), -32768);
        // And each message is its own axis.
        assert_eq!(wheel_motion(WM_MOUSEWHEEL, -120), (0, -120));
        assert_eq!(wheel_motion(WM_MOUSEHWHEEL, 120), (120, 0));
    }

    /// **Relative raw input is the motion itself; absolute is a difference of positions**, scaled
    /// from `0..=65535` to the extent, with no motion for the first.
    #[test]
    fn raw_motion_is_relative_counts_or_the_difference_of_absolute_positions() {
        let mut last = None;
        assert_eq!(raw_motion(0, 7, -3, &mut last, (1920, 1080)), (7, -3));
        assert_eq!(last, None);
        // Absolute across a 1920x1080 primary: the first report is a position, not a motion.
        assert_eq!(raw_motion(MOUSE_MOVE_ABSOLUTE, 32768, 32768, &mut last, (1920, 1080)), (0, 0));
        assert_eq!(last, Some((960, 540)));
        assert_eq!(raw_motion(MOUSE_MOVE_ABSOLUTE, 65535, 0, &mut last, (1920, 1080)), (960, -540));
        assert_eq!(last, Some((1920, 0)));
        // A relative report between two absolute ones forgets the last position.
        assert_eq!(raw_motion(0, 1, 1, &mut last, (1920, 1080)), (1, 1));
        assert_eq!(raw_motion(MOUSE_MOVE_ABSOLUTE, 0, 0, &mut last, (1920, 1080)), (0, 0));
        // A degenerate extent is one pixel, not a division by zero.
        let mut last = None;
        raw_motion(MOUSE_MOVE_ABSOLUTE | MOUSE_VIRTUAL_DESKTOP, 100, 100, &mut last, (0, -5));
        assert_eq!(last, Some((0, 0)));
    }

    /// **The wheel, through the window procedure**: posted `WM_MOUSEWHEEL` and `WM_MOUSEHWHEEL`
    /// come out as [`WindowEvent::Wheel`] on their own axis, signed, at the pointer's position
    /// converted from the **screen** coordinates the two messages carry to the client's.
    #[test]
    #[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
    fn posted_wheel_messages_come_out_as_wheel_events_in_client_coordinates() {
        assert!(
            std::env::var("OMNI_GFX_WINDOW_TESTS").is_ok_and(|v| v == "1"),
            "run with --ignored but OMNI_GFX_WINDOW_TESTS is not 1; this creates a real window"
        );
        let mut window = Window::create(&WindowDesc::new("omnidroid: wheel", 320, 240)).unwrap();
        let mut origin = POINT { x: 0, y: 0 };
        // SAFETY: a live window handle and a `POINT` written in place.
        unsafe { ClientToScreen(window.hwnd, &raw mut origin) };
        let screen = |x: i32, y: i32| -> LPARAM {
            let (x, y) = (origin.x + x, origin.y + y);
            ((y as u16 as u32) << 16 | x as u16 as u32) as i32 as LPARAM
        };
        let notch_away: WPARAM = 120 << 16;
        let notch_towards: WPARAM = (0xFF88 << 16) | 0x0008;
        for (msg, wparam, lparam) in [
            (WM_MOUSEWHEEL, notch_away, screen(10, 20)),
            (WM_MOUSEWHEEL, notch_towards, screen(30, 40)),
            (WM_MOUSEHWHEEL, notch_away, screen(-5, 7)),
        ] {
            // SAFETY: a live window handle and a message with no pointer arguments.
            assert_ne!(unsafe { PostMessageW(window.hwnd, msg, wparam, lparam) }, 0, "{msg:#x}");
        }
        let mut events = Vec::new();
        window.poll(&mut events);
        events.retain(|e| matches!(e, WindowEvent::Wheel { .. }));
        assert_eq!(events, [
            WindowEvent::Wheel { x: 10, y: 20, dx: 0, dy: 120 },
            WindowEvent::Wheel { x: 30, y: 40, dx: 0, dy: -120 },
            WindowEvent::Wheel { x: -5, y: 7, dx: 120, dy: 0 },
        ]);
    }

    /// **A display change, through the window procedure**: `WM_DISPLAYCHANGE` (what Windows
    /// broadcasts when another program switches the display's mode) and `WM_DPICHANGED` come out
    /// as [`WindowEvent::DisplayChanged`] naming which, each one, and with no `Resized` of their
    /// own: the client size did not change.
    #[test]
    #[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
    fn display_changes_come_out_as_display_changed_events() {
        use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;
        assert!(
            std::env::var("OMNI_GFX_WINDOW_TESTS").is_ok_and(|v| v == "1"),
            "run with --ignored but OMNI_GFX_WINDOW_TESTS is not 1; this creates a real window"
        );
        let mut window = Window::create(&WindowDesc::new("omnidroid: display", 320, 240)).unwrap();
        let mut drained = Vec::new();
        window.poll(&mut drained);
        let mut outer = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: writes a `RECT`; the handle is live.
        assert_ne!(unsafe { GetWindowRect(window.hwnd, &raw mut outer) }, 0);
        // 32 bits per pixel, 1920x1080: what the message carries; nothing here reads it.
        let mode: LPARAM = (1080 << 16) | 1920;
        // SAFETY: a live window handle; `WM_DISPLAYCHANGE` carries no pointer, and
        // `WM_DPICHANGED`'s `LPARAM` is the window's own rectangle, live across the call.
        unsafe {
            SendMessageW(window.hwnd, WM_DISPLAYCHANGE, 32, mode);
            SendMessageW(window.hwnd, WM_DPICHANGED, (144 << 16) | 144, (&raw const outer) as LPARAM);
            SendMessageW(window.hwnd, WM_DISPLAYCHANGE, 32, mode);
        }
        let mut events = Vec::new();
        window.poll(&mut events);
        events.retain(|e| !matches!(e, WindowEvent::FocusChanged { .. } | WindowEvent::PointerMoved { .. }));
        assert_eq!(events, [
            WindowEvent::DisplayChanged { change: DisplayChange::Mode },
            WindowEvent::DisplayChanged { change: DisplayChange::Scale },
            WindowEvent::DisplayChanged { change: DisplayChange::Mode },
        ]);
    }

    /// **`wait` sleeps until something arrives, and no longer**: with nothing queued it returns
    /// `false` after the timeout and not before; with a message posted it returns `true` at once,
    /// and the message is still there for the poll.
    #[test]
    #[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
    fn wait_returns_when_a_message_arrives_and_times_out_without_one() {
        use std::time::{Duration, Instant};
        assert!(
            std::env::var("OMNI_GFX_WINDOW_TESTS").is_ok_and(|v| v == "1"),
            "run with --ignored but OMNI_GFX_WINDOW_TESTS is not 1; this creates a real window"
        );
        let mut window = Window::create(&WindowDesc::new("omnidroid: wait", 320, 240)).unwrap();
        let mut drained = Vec::new();
        window.poll(&mut drained);
        let started = Instant::now();
        assert!(!window.wait(Duration::from_millis(60)), "nothing was queued");
        assert!(started.elapsed() >= Duration::from_millis(50), "returned early: {:?}", started.elapsed());
        // SAFETY: a live window handle and a message with no pointer arguments.
        assert_ne!(unsafe { PostMessageW(window.hwnd, WM_CHAR, 0x61, 1) }, 0);
        let started = Instant::now();
        assert!(window.wait(Duration::from_secs(5)), "a message was posted");
        assert!(started.elapsed() < Duration::from_secs(1), "slept past it: {:?}", started.elapsed());
        drained.clear();
        window.poll(&mut drained);
        assert_eq!(drained, [WindowEvent::Text { text: "a".to_owned() }]);
    }

    /// **A capture pins the cursor to one pixel of the client area and gives it back**: requested
    /// on an unfocused window it is declined and nothing is clipped; granted on the focused one,
    /// the clip is the one pixel under the cursor; released, the clip is what it was before; and
    /// the focus leaving (`WM_KILLFOCUS` sent to the window procedure) ends it and says so.
    ///
    /// It takes the foreground, and briefly the cursor, which is why it is gated.
    #[test]
    #[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
    fn a_capture_pins_the_cursor_and_the_focus_leaving_ends_it() {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetClipCursor, SendMessageW, SetForegroundWindow,
        };
        assert!(
            std::env::var("OMNI_GFX_WINDOW_TESTS").is_ok_and(|v| v == "1"),
            "run with --ignored but OMNI_GFX_WINDOW_TESTS is not 1; this creates a real window"
        );
        let clip = || {
            let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            // SAFETY: writes a `RECT`.
            assert_ne!(unsafe { GetClipCursor(&raw mut rect) }, 0);
            (rect.left, rect.top, rect.right, rect.bottom)
        };
        let before = clip();

        // Never shown, so never focused: declined, and nothing clipped.
        let mut hidden = Window::create(&WindowDesc::new("omnidroid: unfocused", 320, 240)).unwrap();
        assert!(!hidden.set_pointer_capture(true).unwrap(), "declined without the focus");
        assert!(!hidden.has_pointer_capture());
        assert_eq!(clip(), before);
        drop(hidden);

        let mut window = Window::create(&WindowDesc::new("omnidroid: capture", 320, 240)).unwrap();
        window.show();
        // SAFETY: live window handle.
        unsafe {
            SetForegroundWindow(window.hwnd);
            SetFocus(window.hwnd);
        }
        let mut drained = Vec::new();
        window.poll(&mut drained);
        // SAFETY: no arguments.
        let focused = unsafe { GetFocus() };
        assert_eq!(
            focused,
            window.hwnd,
            "the test window could not take the focus (another window holds the foreground)"
        );
        assert!(window.set_pointer_capture(true).unwrap(), "granted with the focus");
        assert!(window.has_pointer_capture());
        let (left, top, right, bottom) = clip();
        assert_eq!((right - left, bottom - top), (1, 1), "pinned to one pixel");
        let mut origin = POINT { x: 0, y: 0 };
        // SAFETY: a live window handle and a `POINT` written in place.
        unsafe { ClientToScreen(window.hwnd, &raw mut origin) };
        let (width, height) = window.client_size().unwrap();
        assert!(
            (origin.x..origin.x + width as i32).contains(&left)
                && (origin.y..origin.y + height as i32).contains(&top),
            "the pixel ({left}, {top}) is inside the client area at {origin:?}, {width}x{height}",
            origin = (origin.x, origin.y)
        );
        // Asking again is a no-op; releasing gives the clip back.
        assert!(window.set_pointer_capture(true).unwrap());
        assert!(!window.set_pointer_capture(false).unwrap());
        assert!(!window.has_pointer_capture());
        assert_eq!(clip(), before);

        // Captured again, then the focus leaves: ended, reported, and unclipped.
        assert!(window.set_pointer_capture(true).unwrap());
        window.poll(&mut drained);
        drained.clear();
        // SAFETY: a live window handle; `WM_KILLFOCUS` carries a window handle, null here.
        unsafe { SendMessageW(window.hwnd, WM_KILLFOCUS, 0, 0) };
        window.poll(&mut drained);
        assert!(!window.has_pointer_capture());
        assert_eq!(clip(), before);
        let lost = drained.iter().position(|e| *e == WindowEvent::PointerCaptureLost);
        let unfocused = drained.iter().position(|e| *e == WindowEvent::FocusChanged { focused: false });
        assert!(
            matches!((lost, unfocused), (Some(a), Some(b)) if a < b),
            "the capture's end is reported, before the focus change: {drained:?}"
        );
    }

    /// **A hidden cursor is hidden over the client area, focused or not** (point 6): asked for it
    /// is gone at once, without a move, and `WM_SETCURSOR` keeps it gone over the client area but
    /// not over the frame; **the focus leaving does not show it** -- the owner's w33: a Roblox
    /// cursor following the pointer over an inactive window beside the host's -- and neither does
    /// its coming back; a capture given back after the held point was moved (`warp_pointer`) leaves
    /// it hidden, there, and reports that point; and withdrawing the request shows it.
    ///
    /// `GetCursor` is the cursor this thread last set, which is the one shown while the pointer is
    /// over this thread's window -- the instrument reads what the window procedure did, not the
    /// backend's own flag (VERIFICATION entry 7). It takes the foreground and pins the cursor for
    /// a moment, which is why it is gated.
    #[test]
    #[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
    fn a_hidden_cursor_is_hidden_over_the_client_area_focused_or_not() {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetCursor, HTCAPTION, SendMessageW, SetForegroundWindow, WM_MOUSEMOVE,
        };
        assert!(
            std::env::var("OMNI_GFX_WINDOW_TESTS").is_ok_and(|v| v == "1"),
            "run with --ignored but OMNI_GFX_WINDOW_TESTS is not 1; this creates a real window"
        );
        let mut window = Window::create(&WindowDesc::new("omnidroid: hidden cursor", 320, 240)).unwrap();
        window.show();
        // SAFETY: live window handle. **Topmost**, because the cursor's position has to be over this
        // window: MEASURED, an always-on-top window of the desktop's (a `Chrome_RenderWidgetHostHWND`)
        // covered the point where the window landed.
        unsafe {
            SetWindowPos(
                window.hwnd,
                windows_sys::Win32::UI::WindowsAndMessaging::HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOSIZE,
            );
            SetForegroundWindow(window.hwnd);
            SetFocus(window.hwnd);
        }
        let mut drained = Vec::new();
        window.poll(&mut drained);
        // SAFETY: no arguments.
        let focused = unsafe { GetFocus() };
        assert_eq!(
            focused,
            window.hwnd,
            "the test window could not take the focus (another window holds the foreground)"
        );
        assert!(window.has_focus(), "the first focus, which arrived while the window was shown");
        let mut origin = POINT { x: 0, y: 0 };
        // SAFETY: a live window handle and a `POINT` written in place.
        unsafe { ClientToScreen(window.hwnd, &raw mut origin) };
        let (width, height) = window.client_size().unwrap();
        let (cx, cy) = (width as i32 / 2, height as i32 / 2);
        // **The cursor is pinned to the client area's centre for the whole test**, with the clip a
        // capture uses: MEASURED, with a person at the desktop moving the mouse, a cursor merely
        // put there had left before the first assertion (8 runs of 9). Lifted however the test
        // ends.
        struct Unclip;
        impl Drop for Unclip {
            fn drop(&mut self) {
                // SAFETY: a null rectangle lifts the clip.
                unsafe { ClipCursor(core::ptr::null()) };
            }
        }
        let centre = POINT { x: origin.x + cx, y: origin.y + cy };
        // SAFETY: by-value coordinates.
        unsafe { SetCursorPos(centre.x, centre.y) };
        clip_to(centre).expect("pin the cursor");
        let _unclip = Unclip;
        let hwnd = window.hwnd;
        if !cursor_over_client(hwnd) {
            let mut at = POINT { x: 0, y: 0 };
            let mut name = [0u16; 128];
            // SAFETY: writes a `POINT`; by-value point; a buffer and its length.
            let (under, length) = unsafe {
                GetCursorPos(&raw mut at);
                let under = WindowFromPoint(at);
                (under, windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW(under, name.as_mut_ptr(), 128))
            };
            panic!(
                "the cursor, pinned at {:?}, is at {:?} over {under:?} ({}), not this window {hwnd:?}",
                (centre.x, centre.y),
                (at.x, at.y),
                String::from_utf16_lossy(&name[..length.max(0) as usize])
            );
        }
        // What the system sends when the pointer moves over the window, at a hit-test code.
        let set_cursor = |hit: u32| {
            // SAFETY: a live window handle; `WM_SETCURSOR` carries a handle and two codes.
            unsafe { SendMessageW(hwnd, WM_SETCURSOR, hwnd as WPARAM, (hit | (WM_MOUSEMOVE << 16)) as LPARAM) };
        };
        // SAFETY: no arguments, here and below.
        let visible = || !unsafe { GetCursor() }.is_null();

        set_cursor(HTCLIENT);
        assert!(visible(), "the arrow, before anything was asked");
        assert!(!window.cursor_hidden());

        window.set_cursor_hidden(true).unwrap();
        assert!(window.cursor_hidden());
        assert!(!visible(), "hidden at once, without waiting for a move");
        set_cursor(HTCLIENT);
        assert!(!visible(), "and WM_SETCURSOR keeps it hidden over the client area");
        set_cursor(HTCAPTION);
        assert!(visible(), "but not over the title bar");
        set_cursor(HTCLIENT);
        assert!(!visible());

        // SAFETY: a live window handle; the focus messages carry a window handle, null here.
        unsafe { SendMessageW(hwnd, WM_KILLFOCUS, 0, 0) };
        assert!(!window.has_focus());
        assert!(window.cursor_hidden(), "the request stands without the focus");
        assert!(!visible(), "the focus leaving does not show it over the client area");
        set_cursor(HTCLIENT);
        assert!(!visible(), "and WM_SETCURSOR keeps it hidden over an inactive window");
        set_cursor(HTCAPTION);
        assert!(visible(), "but not over its title bar");
        // SAFETY: as above.
        unsafe { SendMessageW(hwnd, WM_SETFOCUS, 0, 0) };
        assert!(window.has_focus());
        set_cursor(HTCLIENT);
        assert!(!visible());

        // A capture whose held point is moved and then given back: hidden throughout, the cursor
        // where it was moved to, and that point reported.
        assert!(window.set_pointer_capture(true).unwrap());
        drained.clear();
        window.poll(&mut drained);
        let moved = (cx - 20, cy + 10);
        window.warp_pointer(moved.0, moved.1).unwrap();
        assert!(!visible(), "hidden while held and moved");
        assert!(!window.set_pointer_capture(false).unwrap());
        assert!(!visible(), "still hidden after the capture: the request stands");
        let mut at = POINT { x: 0, y: 0 };
        // SAFETY: writes a `POINT`.
        unsafe { GetCursorPos(&raw mut at) };
        assert_eq!((at.x - origin.x, at.y - origin.y), moved, "the cursor is where it was moved to");
        // SAFETY: by-value coordinates.
        unsafe { SetCursorPos(centre.x, centre.y) };
        clip_to(centre).expect("pin the cursor again: the release lifted the clip");
        drained.clear();
        window.poll(&mut drained);
        assert!(
            drained.contains(&WindowEvent::PointerMoved { x: moved.0, y: moved.1 }),
            "the release reports the point it held: {drained:?}"
        );

        window.set_cursor_hidden(false).unwrap();
        assert!(!window.cursor_hidden());
        assert!(visible(), "withdrawn: shown at once");
        set_cursor(HTCLIENT);
        assert!(visible());
    }

    /// **A button reported down is reported up however its release is lost** (point 7): a press
    /// the physical mouse never made -- posted, so the host's own button state says up -- is
    /// released by the next poll, which asks it; and a press whose mouse capture another window
    /// takes (`WM_CAPTURECHANGED`, as a system menu or `WM_CANCELMODE` causes) is released at once,
    /// once. The w33 right-drag that kept turning the camera is the reason.
    ///
    /// Assumes nobody holds a mouse button during the test. Gated: it creates a real window.
    #[test]
    #[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
    fn a_button_whose_release_is_lost_is_released() {
        use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;
        assert!(
            std::env::var("OMNI_GFX_WINDOW_TESTS").is_ok_and(|v| v == "1"),
            "run with --ignored but OMNI_GFX_WINDOW_TESTS is not 1; this creates a real window"
        );
        let mut window = Window::create(&WindowDesc::new("omnidroid: lost release", 320, 240)).unwrap();
        let mut drained = Vec::new();
        window.poll(&mut drained);
        let at = |x: i32, y: i32| ((y << 16) | (x & 0xffff)) as LPARAM;

        // Pressed, never released: the host says the right button is up.
        // SAFETY: a live window handle; the message carries a key mask and a position.
        unsafe { SendMessageW(window.hwnd, WM_RBUTTONDOWN, 0x0002, at(40, 30)) };
        drained.clear();
        window.poll(&mut drained);
        assert_eq!(
            drained,
            [
                WindowEvent::PointerDown { button: PointerButton::Secondary, x: 40, y: 30 },
                WindowEvent::PointerUp { button: PointerButton::Secondary, x: 40, y: 30 },
            ],
            "the poll released a button the host says is up"
        );
        drained.clear();
        window.poll(&mut drained);
        assert!(drained.is_empty(), "and only once: {drained:?}");

        // Pressed, then the capture taken away before any poll: released by that, once.
        // SAFETY: as above; `WM_CAPTURECHANGED` names the window taking it, none here.
        unsafe {
            SendMessageW(window.hwnd, WM_LBUTTONDOWN, 0x0001, at(10, 20));
            SendMessageW(window.hwnd, WM_CAPTURECHANGED, 0, 0);
        }
        // SAFETY: live for as long as `window`; a read, with the window procedure not running.
        assert_eq!(unsafe { (*window.state).buttons_down }, 0, "released by the capture's loss itself, before any poll");
        drained.clear();
        window.poll(&mut drained);
        assert_eq!(
            drained,
            [
                WindowEvent::PointerDown { button: PointerButton::Primary, x: 10, y: 20 },
                WindowEvent::PointerUp { button: PointerButton::Primary, x: 10, y: 20 },
            ],
            "the capture's loss released it, and the poll found nothing more to release"
        );
    }

    /// The physical key of each button, through the swap setting.
    #[test]
    fn a_buttons_physical_key_follows_the_swap_setting() {
        assert_eq!(physical_key(PointerButton::Primary, false), VK_LBUTTON);
        assert_eq!(physical_key(PointerButton::Secondary, false), VK_RBUTTON);
        assert_eq!(physical_key(PointerButton::Primary, true), VK_RBUTTON);
        assert_eq!(physical_key(PointerButton::Secondary, true), VK_LBUTTON);
        assert_eq!(physical_key(PointerButton::Middle, true), VK_MBUTTON);
        assert_eq!(physical_key(PointerButton::Back, false), VK_XBUTTON1);
        assert_eq!(physical_key(PointerButton::Forward, false), VK_XBUTTON2);
    }

    /// Feed `units` through one window's worth of `WM_CHAR` state, keeping every answer — the
    /// `None`s included, so that a test also pins *which* message produced the text.
    fn feed(units: &[u16]) -> Vec<Option<String>> {
        let mut pending = None;
        units.iter().map(|&unit| text_from_char(&mut pending, unit)).collect()
    }

    /// A character in the Basic Multilingual Plane is its own text, ASCII or not: `a`, `é`
    /// (U+00E9), `ç` (U+00E7), `水` (U+6C34).
    #[test]
    fn a_bmp_character_is_its_own_text() {
        assert_eq!(feed(&[0x61]), [Some("a".to_owned())]);
        assert_eq!(feed(&[0xE9]), [Some("é".to_owned())]);
        assert_eq!(feed(&[0xE7]), [Some("ç".to_owned())]);
        assert_eq!(feed(&[0x6C34]), [Some("水".to_owned())]);
        assert_eq!(feed(&[0x20]), [Some(" ".to_owned())], "space is text, not a control code");
        assert_eq!(feed(&[0x7E]), [Some("~".to_owned())], "the unit just below DEL");
    }

    /// U+1F600 arrives as two `WM_CHAR`s, `0xD83D` then `0xDE00`, and is **one** event, from the
    /// second message.
    #[test]
    fn a_surrogate_pair_is_one_character_from_its_second_half() {
        assert_eq!(feed(&[0xD83D, 0xDE00]), [None, Some("😀".to_owned())]);
        // And the pair leaves nothing behind: the next character is on its own again.
        assert_eq!(feed(&[0xD83D, 0xDE00, 0x61]), [
            None,
            Some("😀".to_owned()),
            Some("a".to_owned())
        ]);
    }

    /// A low surrogate with no high one before it is not a character, and there is nothing it
    /// could be joined to.
    #[test]
    fn a_lone_low_surrogate_is_dropped() {
        assert_eq!(feed(&[0xDE00]), [None]);
        assert_eq!(feed(&[0xDC00, 0x61]), [None, Some("a".to_owned())]);
        assert_eq!(feed(&[0xDFFF]), [None]);
    }

    /// A high surrogate whose next unit is not a low one is dropped, and the next unit is judged on
    /// its own: a character is text, a control code is still nothing, and another high surrogate
    /// starts a pair of its own.
    #[test]
    fn a_high_surrogate_without_its_partner_is_dropped_and_the_next_unit_stands_alone() {
        assert_eq!(feed(&[0xD83D, 0x61]), [None, Some("a".to_owned())]);
        assert_eq!(feed(&[0xD83D, 0x0D]), [None, None]);
        assert_eq!(feed(&[0xD83D, 0xD83D, 0xDE00]), [None, None, Some("😀".to_owned())]);
        // Held forever is also dropped: the stream just ends.
        assert_eq!(feed(&[0xDBFF]), [None]);
    }

    /// Every C0 control code and DEL are keys, not text — Backspace, Tab, Enter, Escape and the
    /// Ctrl+letter codes among them — and so is none of them after a held high surrogate.
    #[test]
    fn control_codes_and_del_are_not_text() {
        for unit in (0x00..=0x1F).chain([0x7F]) {
            assert_eq!(feed(&[unit]), [None], "unit {unit:#04x} must not be text");
            assert_eq!(feed(&[0xD83D, unit]), [None, None], "unit {unit:#04x} after a high half");
        }
    }

    /// **The window procedure's `WM_CHAR` arm, end to end**: posted characters come out of
    /// [`Window::poll`] as [`WindowEvent::Text`] — a surrogate pair as one, a control code and a
    /// lone low surrogate as none — and behind the key posted before them.
    ///
    /// Posted rather than typed, so it needs neither a keyboard nor a particular layout. The key
    /// is the Left arrow because `TranslateMessage` makes no character of it, so the only
    /// `WM_CHAR`s the window sees are the ones posted here. Gated like `tests/window_live.rs`, and
    /// for the same reason: it creates a real window.
    #[test]
    #[ignore = "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
    fn posted_characters_come_out_as_text_behind_the_key_before_them() {
        assert!(
            std::env::var("OMNI_GFX_WINDOW_TESTS").is_ok_and(|v| v == "1"),
            "run with --ignored but OMNI_GFX_WINDOW_TESTS is not 1; this creates a real window"
        );
        let mut window = Window::create(&WindowDesc::new("omnidroid: text", 320, 240)).unwrap();
        // Left arrow: VK_LEFT, make code 0x4B with the extended flag, repeat count 1.
        let mut posts: Vec<(u32, WPARAM, LPARAM)> = vec![(WM_KEYDOWN, 0x25, 0x014B_0001)];
        for unit in [0xE9, 0x0D, 0xD83D, 0xDE00, 0xDC00, 0xE7] {
            posts.push((WM_CHAR, unit, 1));
        }
        for (msg, wparam, lparam) in posts {
            // SAFETY: a live window handle and a message with no pointer arguments.
            let posted = unsafe { PostMessageW(window.hwnd, msg, wparam, lparam) };
            assert_ne!(posted, 0, "PostMessageW({msg:#x}, {wparam:#x}) failed");
        }

        let mut events = Vec::new();
        window.poll(&mut events);
        events.retain(|e| matches!(e, WindowEvent::KeyDown { .. } | WindowEvent::Text { .. }));
        assert_eq!(events, [
            WindowEvent::KeyDown { keycode: 0x25, scancode: 0xE04B, repeat: false },
            WindowEvent::Text { text: "é".to_owned() },
            WindowEvent::Text { text: "😀".to_owned() },
            WindowEvent::Text { text: "ç".to_owned() },
        ]);
    }
}
