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

use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, GetWindowRect, IDC_ARROW, LoadCursorW, MSG,
    PM_REMOVE, PeekMessageW, PostMessageW, RegisterClassW, SW_MINIMIZE, SW_RESTORE,
    SW_SHOWNORMAL, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    WM_CAPTURECHANGED, WM_CLOSE, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SETFOCUS, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSW, WS_OVERLAPPEDWINDOW, XBUTTON1,
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
            push_event(&mut state.queue, WindowEvent::FocusChanged {
                focused: msg == WM_SETFOCUS,
            });
        }
        WM_MOUSEMOVE => {
            let (x, y) = mouse_xy(lparam);
            push_event(&mut state.queue, WindowEvent::PointerMoved { x, y });
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
        _ => {}
    }

    // Everything above except `WM_CLOSE` still wants the default behaviour: `WM_SIZE` and the
    // focus messages have real work behind them, and swallowing the key messages would break
    // `Alt+F4` and the system menu.
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
            // SAFETY: `msg` was just filled by `PeekMessageW`. `TranslateMessage` only posts
            // `WM_CHAR` for key messages — unused today, and the reason it is here is that a
            // backend without it makes character input silently impossible rather than merely
            // unimplemented. `DispatchMessageW` re-enters `wnd_proc`, which is why no reference
            // into `*self.state` is held across this call.
            unsafe {
                TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        // SAFETY: `self.state` is live for as long as `self` is, and the pump above has returned,
        // so the window procedure is not running and holds no reference into it.
        let state = unsafe { &mut *self.state };
        sink.append(&mut state.queue);
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
        // SAFETY: a live window handle, destroyed from the thread that created it — which is the
        // only thread that can hold a `Window`, because it is `!Send`.
        unsafe { DestroyWindow(self.hwnd) };
        // SAFETY: `state` came from `Box::into_raw` in `create` and nothing else owns it. The
        // window is gone, so `wnd_proc` can no longer be entered for it.
        drop(unsafe { Box::from_raw(self.state) });
    }
}
