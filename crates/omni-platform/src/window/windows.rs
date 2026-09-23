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

use std::sync::OnceLock;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, ReleaseCapture, SetCapture};
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
    RegisterClassW, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SW_MINIMIZE,
    SW_RESTORE, SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetCursor, SetCursorPos,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, WM_CAPTURECHANGED, WM_CHAR,
    WM_CLOSE, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE,
    WM_NCDESTROY, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS, WM_SIZE, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WS_OVERLAPPEDWINDOW, XBUTTON1,
};

use super::{
    PointerButton, RawWindow, WindowDesc, WindowError, WindowEvent, WindowResult, push_event,
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
/// cleared. The cursor's visibility comes back by itself -- it is hidden only from `WM_SETCURSOR`
/// while `captured` is set.
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
            if msg == WM_KILLFOCUS && state.captured.is_some() {
                end_capture(state);
                push_event(&mut state.queue, WindowEvent::PointerCaptureLost);
            }
            push_event(&mut state.queue, WindowEvent::FocusChanged {
                focused: msg == WM_SETFOCUS,
            });
        }
        WM_MOUSEMOVE => {
            // Not while captured: the cursor is pinned, and what moves is `WM_INPUT`'s.
            if state.captured.is_none() {
                let (x, y) = mouse_xy(lparam);
                push_event(&mut state.queue, WindowEvent::PointerMoved { x, y });
            }
        }
        WM_SETCURSOR => {
            // Hidden over the client area while captured; the class's arrow everywhere else, and
            // over the client area otherwise, from `DefWindowProcW`.
            if state.captured.is_some() && (lparam & 0xffff) as u32 == HTCLIENT {
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
            state.buttons_down &= !(1 << button_bit(button));
            if state.buttons_down == 0 {
                // SAFETY: no arguments; a no-op when this thread holds no capture.
                unsafe { ReleaseCapture() };
            }
            push_event(&mut state.queue, WindowEvent::PointerUp { button, x, y });
        }
        WM_CAPTURECHANGED => {
            // Something else took the capture — a system drag, a menu, another window. The mask
            // is now a lie, and a lie that never clears: every later button-up would find bits
            // still set and never release. Reset it rather than tracking who has what.
            state.buttons_down = 0;
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
        sink.append(&mut state.queue);
    }

    /// See [`super::Window::set_pointer_capture`] and this module's point 5.
    pub(super) fn set_pointer_capture(&mut self, captured: bool) -> WindowResult<bool> {
        // SAFETY: as in `poll`: live for as long as `self`, and the window procedure is not
        // running -- nothing below sends this window a message.
        let state = unsafe { &mut *self.state };
        if !captured {
            if state.captured.is_some() {
                end_capture(state);
                // Visible again at once, where it was held, rather than at the next move.
                // SAFETY: a system cursor, as `window_class` loads it.
                unsafe { SetCursor(LoadCursorW(core::ptr::null_mut(), IDC_ARROW)) };
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
        end_capture(unsafe { &mut *self.state });
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
