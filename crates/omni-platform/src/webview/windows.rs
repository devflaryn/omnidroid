//! Windows backend for the web-view seam: Microsoft Edge WebView2 on a thread of its own.
//!
//! # The thread, in order
//!
//! [`WebView::open`] finds the runtime and its creation export on the caller's thread (so that a
//! missing runtime is `open`'s error), then starts `omni-webview`, which:
//!
//! 1. `CoInitializeEx(COINIT_APARTMENTTHREADED)` — WebView2 requires a single-threaded apartment;
//! 2. creates the window (class `Omnidroid.WebView`, `WS_OVERLAPPEDWINDOW`, the requested client
//!    size measured and corrected exactly as `window/windows.rs` does it), shows it, and tells
//!    `open` it may return;
//! 3. calls `CreateWebViewEnvironmentWithOptionsInternal` (see `loader.rs`) with the user data
//!    folder `%LOCALAPPDATA%\Omnidroid\webview`;
//! 4. pumps messages with `GetMessageW` until the window is destroyed.
//!
//! Everything after that is callbacks through the pump: the environment's completion creates the
//! controller; the controller's completion sizes it to the client area, registers the four event
//! handlers, and adds the init script; the script's completion sends [`WebViewEvent::Ready`],
//! issues the first navigation and runs whatever commands arrived meanwhile.
//!
//! # State, and why nothing is borrowed across a call
//!
//! The thread's state is one [`Ui`] behind an `Rc`, reached by the window procedure through
//! `GWLP_USERDATA` and by every WebView2 callback through a `Weak` (a callback that outlives the
//! state finds nothing and does nothing). WebView2 calls back on this thread, sometimes from inside
//! a call this thread made, so **no `RefCell` borrow is ever held across a COM call**: an interface
//! is cloned (`AddRef`) out of its cell and called on the clone. A borrow held across a re-entrant
//! call would be a `BorrowMutError` panic, and a panic here aborts the process from inside an
//! `extern "system"` function.
//!
//! # Commands and the window handle
//!
//! A command is sent on a channel and the pump is woken with `PostMessageW(hwnd, WM_WAKE)`. Posting
//! to an `HWND` after its window is destroyed would post to **whatever window reuses the handle**,
//! so the handle is published in a mutex that the thread clears before it destroys the window, and
//! a poster posts only while holding the mutex and seeing it set. That is also what makes a command
//! after the close answer [`WebViewError::Closed`].

mod com;
mod loader;

use core::ffi::c_void;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, ERROR_ENVVAR_NOT_FOUND, GetLastError, HWND, LPARAM, LRESULT, RECT, S_FALSE,
    S_OK, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowRect, IDC_ARROW,
    LoadCursorW, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SW_SHOWNORMAL, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOZORDER, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateMessage, WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CLOSE, WM_DESTROY, WM_NCCREATE,
    WM_NCDESTROY, WM_SETFOCUS, WM_SIZE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows_sys::core::HRESULT;

use self::com::{
    AddHandler, Com, ControllerVtbl, CoreWebView2Vtbl, EnvironmentVtbl, EventRegistrationToken,
    HandlerRef, IID_AddScriptCompleted, IID_ControllerCompleted, IID_EnvironmentCompleted,
    IID_ExecuteScriptCompleted, IID_NavigationCompletedHandler, IID_NavigationStartingHandler,
    IID_ProcessFailedHandler, IID_WebMessageReceivedHandler, MOVE_FOCUS_REASON_PROGRAMMATIC,
    IID_Settings2, IUnknownVtbl, NavigationCompletedArgsVtbl, NavigationStartingArgsVtbl,
    ProcessFailedArgsVtbl, RemoveHandler, Settings2Vtbl, WebMessageArgsVtbl, co_string, guarded,
};
use self::loader::CreateEnvironment;
use super::{Command, WebViewError, WebViewEvent, WebViewOptions, WebViewResult};

/// Wakes the pump to read the command channel.
const WM_WAKE: u32 = WM_APP + 1;

// ---------------------------------------------------------------------------------------------
// Small pure helpers
// ---------------------------------------------------------------------------------------------

/// NUL-terminated UTF-16. Interior NULs were refused by `super::validate` before anything got here.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(core::iter::once(0)).collect()
}

/// NUL-terminated UTF-16 of a path, exactly — not through a lossy `String`.
fn wide_os(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(core::iter::once(0)).collect()
}

/// `HRESULT_FROM_WIN32` from `winerror.h`, as `audio/windows.rs` transcribes it.
const fn hresult_from_win32(code: u32) -> HRESULT {
    let as_hresult = code as HRESULT;
    if as_hresult <= 0 { as_hresult } else { ((code & 0xFFFF) | (7 << 16) | 0x8000_0000) as HRESULT }
}

/// `GetLastError` as a [`WebViewError::Os`] on the `HRESULT` scale.
fn last_error(operation: &'static str, api: &'static str) -> WebViewError {
    // SAFETY: no arguments and no memory; the caller calls this straight after the failing API.
    let code = unsafe { GetLastError() };
    WebViewError::Os { operation, api, code: hresult_from_win32(code) }
}

/// What a `WebMessageReceived` is, given `TryGetWebMessageAsString`'s answer and, only if that says
/// "not a string", `get_WebMessageAsJson`'s.
///
/// WebView2.idl: `TryGetWebMessageAsString` "fails with `E_INVALIDARG`" when the posted value is
/// not a string. Any other failure is the host failing, and says so.
fn message_event(
    string: Result<String, HRESULT>,
    json: impl FnOnce() -> Result<String, HRESULT>,
) -> WebViewEvent {
    match string {
        Ok(text) => WebViewEvent::Message(text),
        Err(E_INVALIDARG) => match json() {
            Ok(json) => WebViewEvent::NonStringMessage { json },
            Err(code) => WebViewEvent::Failed(format!(
                "WebMessageReceived: a non-string message whose get_WebMessageAsJson failed with \
                 HRESULT {code:#010x}"
            )),
        },
        Err(code) => WebViewEvent::Failed(format!(
            "WebMessageReceived: TryGetWebMessageAsString failed with HRESULT {code:#010x}"
        )),
    }
}

/// What a `ProcessFailed` of `kind` (`COREWEBVIEW2_PROCESS_FAILED_KIND`) is.
///
/// Reported: the browser process (the web view is dead), the page's renderer exiting or hanging,
/// a frame's renderer exiting (an iframe — possibly the challenge — replaced by an error page), and
/// an unknown kind. **Not** reported: the utility, sandbox-helper, GPU and plugin processes, of
/// which WebView2.idl says the process "is recreated automatically" or the failure "is not fatal",
/// and the application "does **not** need to handle recovery" — a `Failed` for those would invite
/// the caller to close a page that is still working.
fn process_failed_event(kind: i32) -> Option<WebViewEvent> {
    let (name, consequence) = match kind {
        0 => ("BROWSER_PROCESS_EXITED", "the web view is closed and must be reopened"),
        1 => ("RENDER_PROCESS_EXITED", "the page was replaced by an error page"),
        2 => ("RENDER_PROCESS_UNRESPONSIVE", "the page is not responding"),
        3 => ("FRAME_RENDER_PROCESS_EXITED", "some iframes were replaced by an error page"),
        4..=8 => return None,
        _ => ("an unknown kind", "a browser process ended unexpectedly"),
    };
    Some(WebViewEvent::Failed(format!("ProcessFailed: {name} ({kind}): {consequence}")))
}

/// A mutex whose data is a plain value that no panic can leave half-written.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// Process-wide window setup
// ---------------------------------------------------------------------------------------------

/// Per-monitor DPI awareness, once, before this module's first window: the window seam's
/// `declare_dpi_awareness`, repeated because that one is private to it. Whichever seam makes the
/// first window sets it and the other's call fails with `ERROR_ACCESS_DENIED`, harmlessly — both
/// ask for the same context.
fn declare_dpi_awareness() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // SAFETY: one by-value context constant, no memory.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    });
}

/// The class name, NUL-terminated UTF-16, alive for the process (`RegisterClassW` is not
/// documented to copy it).
fn class_name() -> &'static [u16] {
    static NAME: OnceLock<Vec<u16>> = OnceLock::new();
    NAME.get_or_init(|| wide("Omnidroid.WebView"))
}

/// Register the window class once per process; the atom, or the `GetLastError` code, cached.
fn window_class() -> Result<u16, u32> {
    static CLASS: OnceLock<Result<u16, u32>> = OnceLock::new();
    *CLASS.get_or_init(|| {
        declare_dpi_awareness();
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            // SAFETY: a null name asks for this process's executable image, which always exists.
            hInstance: unsafe { GetModuleHandleW(core::ptr::null()) },
            hIcon: core::ptr::null_mut(),
            // SAFETY: a null instance with an `IDC_*` resource is a shared system cursor.
            hCursor: unsafe { LoadCursorW(core::ptr::null_mut(), IDC_ARROW) },
            // The system window colour (`COLOR_WINDOW + 1`, the documented spelling of a system
            // colour brush) behind the browser, so the client area is not garbage for the second
            // before the browser first paints.
            hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
            lpszMenuName: core::ptr::null(),
            lpszClassName: class_name().as_ptr(),
        };
        // SAFETY: a fully initialised class whose pointers outlive the call (a system cursor and a
        // `'static` name).
        let atom = unsafe { RegisterClassW(&raw const class) };
        if atom == 0 {
            // SAFETY: no arguments; `RegisterClassW` is the last call this thread made.
            return Err(unsafe { GetLastError() });
        }
        Ok(atom)
    })
}

/// `%LOCALAPPDATA%\Omnidroid\webview`, created if missing, as NUL-terminated UTF-16.
fn user_data_folder() -> WebViewResult<Vec<u16>> {
    let Some(base) = std::env::var_os("LOCALAPPDATA") else {
        return Err(WebViewError::Os {
            operation: "open",
            api: "GetEnvironmentVariableW(LOCALAPPDATA)",
            code: hresult_from_win32(ERROR_ENVVAR_NOT_FOUND),
        });
    };
    let folder = PathBuf::from(base).join("Omnidroid").join("webview");
    std::fs::create_dir_all(&folder).map_err(|error| WebViewError::Os {
        operation: "open",
        api: "CreateDirectoryW(%LOCALAPPDATA%\\Omnidroid\\webview)",
        code: error.raw_os_error().map_or(E_FAIL, |code| hresult_from_win32(code as u32)),
    })?;
    Ok(wide_os(folder.as_os_str()))
}

// ---------------------------------------------------------------------------------------------
// The caller's handle
// ---------------------------------------------------------------------------------------------

/// The window handle while the window accepts commands; `None` from the moment its thread begins
/// tearing it down. See this module's "Commands and the window handle".
type Wake = Arc<Mutex<Option<isize>>>;

/// The caller's side: two channels and the thread.
pub(super) struct WebView {
    commands: Sender<Command>,
    events: Receiver<WebViewEvent>,
    wake: Wake,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// Everything the thread is started with.
struct Setup {
    title: Vec<u16>,
    url: String,
    init_script: Option<String>,
    user_agent: Option<String>,
    width: u32,
    height: u32,
    create: CreateEnvironment,
    user_data: Vec<u16>,
    events: Sender<WebViewEvent>,
    commands: Receiver<Command>,
    wake: Wake,
}

pub(super) fn runtime_version() -> WebViewResult<String> {
    loader::locate("runtime_version").map(|runtime| runtime.version)
}

impl WebView {
    pub(super) fn open(options: &WebViewOptions) -> WebViewResult<Self> {
        let runtime = loader::locate("open")?;
        let create = loader::create_environment_export(&runtime)?;
        let user_data = user_data_folder()?;

        let (event_tx, events) = channel();
        let (commands, command_rx) = channel();
        let wake: Wake = Arc::new(Mutex::new(None));
        let setup = Setup {
            title: wide(&options.title),
            url: options.url.clone(),
            init_script: options.init_script.clone(),
            user_agent: options.user_agent.clone(),
            width: options.width,
            height: options.height,
            create,
            user_data,
            events: event_tx,
            commands: command_rx,
            wake: wake.clone(),
        };
        let (opened_tx, opened) = sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("omni-webview".to_owned())
            .spawn(move || run(setup, &opened_tx))
            .map_err(|error| WebViewError::Os {
                operation: "open",
                api: "CreateThread",
                code: error.raw_os_error().map_or(E_FAIL, |code| hresult_from_win32(code as u32)),
            })?;
        match opened.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            // The thread dropped its sender without sending: it panicked. Hand the panic on.
            Err(_) => match thread.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(()) => unreachable!("the web view thread sends on every path that returns"),
            },
        }
        Ok(WebView { commands, events, wake, thread: Mutex::new(Some(thread)) })
    }

    pub(super) fn poll_events(&self) -> Vec<WebViewEvent> {
        self.events.try_iter().collect()
    }

    pub(super) fn send(&self, command: Command, operation: &'static str) -> WebViewResult<()> {
        let wake = lock(&self.wake);
        let Some(hwnd) = *wake else {
            return Err(WebViewError::Closed { operation });
        };
        self.commands.send(command).map_err(|_| WebViewError::Closed { operation })?;
        // SAFETY: the window is live while `wake` holds its handle — the thread clears it, under
        // this same lock, before destroying the window. No pointer arguments.
        if unsafe { PostMessageW(hwnd as HWND, WM_WAKE, 0, 0) } == 0 {
            return Err(last_error(operation, "PostMessageW"));
        }
        Ok(())
    }

    pub(super) fn close(&self) {
        let Some(thread) = lock(&self.thread).take() else {
            return;
        };
        let posted = {
            let wake = lock(&self.wake);
            match *wake {
                // The person already closed it; the thread is ending or has ended.
                None => true,
                Some(hwnd) => {
                    let _ = self.commands.send(Command::Close);
                    // SAFETY: as in `send`.
                    unsafe { PostMessageW(hwnd as HWND, WM_WAKE, 0, 0) != 0 }
                }
            }
        };
        if !posted {
            // The thread cannot be told (a full message queue): joining would wait for the person
            // to close the window. Leave it to end on its own.
            return;
        }
        if thread.join().is_err() && !std::thread::panicking() {
            panic!("the web view's thread panicked; its message is above");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The thread
// ---------------------------------------------------------------------------------------------

/// This thread's `CoInitializeEx(COINIT_APARTMENTTHREADED)`, balanced when dropped.
struct Apartment;

impl Apartment {
    fn enter() -> WebViewResult<Self> {
        // SAFETY: the reserved argument is null; the flag is a constant.
        let hr = unsafe { CoInitializeEx(core::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        match hr {
            S_OK | S_FALSE => Ok(Apartment),
            code => Err(WebViewError::Os { operation: "open", api: "CoInitializeEx", code }),
        }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances `enter` on the same thread; this value never leaves `run`'s frame.
        unsafe { CoUninitialize() };
    }
}

/// A command that waits for the browser.
enum Queued {
    Navigate(String),
    ExecuteScript(String),
}

impl Queued {
    fn operation(&self) -> &'static str {
        match self {
            Queued::Navigate(_) => "navigate",
            Queued::ExecuteScript(_) => "execute_script",
        }
    }
}

/// Where the browser is.
enum Phase {
    /// Environment, controller or init script still on their way.
    Starting,
    /// Ready: the first navigation has been issued. Holds its own reference to the web view.
    Ready(Com<CoreWebView2Vtbl>),
    /// It never became ready, or has been torn down; the reason, for commands that come anyway.
    Broken(String),
}

/// The thread's state. See this module's "State, and why nothing is borrowed across a call".
struct Ui {
    me: Weak<Ui>,
    hwnd: Cell<HWND>,
    url: String,
    init_script: Option<String>,
    user_agent: Option<String>,
    events: Sender<WebViewEvent>,
    commands: Receiver<Command>,
    wake: Wake,
    phase: RefCell<Phase>,
    environment: RefCell<Option<Com<EnvironmentVtbl>>>,
    controller: RefCell<Option<Com<ControllerVtbl>>>,
    webview: RefCell<Option<Com<CoreWebView2Vtbl>>>,
    /// Every event handler registered on `webview`, with the call that removes it.
    registrations: RefCell<Vec<(RemoveHandler, EventRegistrationToken)>>,
    pending: RefCell<VecDeque<Queued>>,
    /// Navigation id to the last URL its `NavigationStarting` named.
    navigations: RefCell<HashMap<u64, String>>,
    closed_by_caller: Cell<bool>,
    torn_down: Cell<bool>,
}

/// The thread's body. Sends exactly one answer on `opened` on every path that returns.
fn run(setup: Setup, opened: &SyncSender<WebViewResult<()>>) {
    let _apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(error) => {
            let _ = opened.send(Err(error));
            return;
        }
    };
    let Setup {
        title,
        url,
        init_script,
        user_agent,
        width,
        height,
        create,
        user_data,
        events,
        commands,
        wake,
    } = setup;
    let ui = Rc::new_cyclic(|me| Ui {
        me: me.clone(),
        hwnd: Cell::new(core::ptr::null_mut()),
        url,
        init_script,
        user_agent,
        events,
        commands,
        wake,
        phase: RefCell::new(Phase::Starting),
        environment: RefCell::new(None),
        controller: RefCell::new(None),
        webview: RefCell::new(None),
        registrations: RefCell::new(Vec::new()),
        pending: RefCell::new(VecDeque::new()),
        navigations: RefCell::new(HashMap::new()),
        closed_by_caller: Cell::new(false),
        torn_down: Cell::new(false),
    });

    let hwnd = match create_window(&title, width, height, Rc::as_ptr(&ui)) {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = opened.send(Err(error));
            return;
        }
    };
    ui.hwnd.set(hwnd);
    *lock(&ui.wake) = Some(hwnd as isize);
    // SAFETY: a live window this thread owns. `SetForegroundWindow` may be refused by the
    // foreground lock (this process may not own the foreground); either way it returns a flag,
    // not an error, and the window is shown regardless.
    unsafe {
        ShowWindow(hwnd, SW_SHOWNORMAL);
        SetForegroundWindow(hwnd);
    }
    let _ = opened.send(Ok(()));

    ui.start(create, &user_data);

    let mut msg = MSG::default();
    // SAFETY: writes a `MSG` at a live pointer; a null window filter takes every message of this
    // thread, which is what WebView2's own windows and posted work need.
    while unsafe { GetMessageW(&raw mut msg, core::ptr::null_mut(), 0, 0) } > 0 {
        // SAFETY: a message `GetMessageW` just filled. `DispatchMessageW` re-enters `wnd_proc` and
        // WebView2's own window procedures.
        unsafe {
            TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
    // `WM_QUIT` comes only from `WM_DESTROY`, after `teardown`; this is then a no-op, and it is
    // the whole of the cleanup if `GetMessageW` ever failed instead.
    ui.teardown();
    if !ui.closed_by_caller.get() {
        let _ = ui.events.send(WebViewEvent::Closed);
    }
}

/// Create the window at a client size of `width` x `height` physical pixels. `state` becomes its
/// `GWLP_USERDATA` from `WM_NCCREATE` on.
fn create_window(title: &[u16], width: u32, height: u32, state: *const Ui) -> WebViewResult<HWND> {
    let atom = window_class().map_err(|code| WebViewError::Os {
        operation: "open",
        api: "RegisterClassW",
        code: hresult_from_win32(code),
    })?;
    // `super::validate` bounded both to 1..=65535, so neither cast can truncate.
    let (want_w, want_h) = (width as i32, height as i32);
    // SAFETY: a registered class atom; a NUL-terminated title that outlives the call; `state` is
    // live for as long as the window (it is the `Rc` `run` holds until after the pump ends).
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            atom as usize as *const u16,
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            want_w,
            want_h,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            GetModuleHandleW(core::ptr::null()),
            state.cast(),
        )
    };
    if hwnd.is_null() {
        return Err(last_error("open", "CreateWindowExW"));
    }
    // The window seam's method: created with the client size as the outer size, then the outer
    // size corrected by the frame actually measured — right at any scale factor.
    let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let mut outer = client;
    // SAFETY: a live window and two writable rectangles.
    let measured = unsafe {
        GetClientRect(hwnd, &raw mut client) != 0 && GetWindowRect(hwnd, &raw mut outer) != 0
    };
    if measured {
        let frame_w = (outer.right - outer.left).saturating_sub(client.right - client.left);
        let frame_h = (outer.bottom - outer.top).saturating_sub(client.bottom - client.top);
        // SAFETY: a live window; `SWP_NOMOVE | SWP_NOZORDER` make the other arguments irrelevant.
        unsafe {
            SetWindowPos(
                hwnd,
                core::ptr::null_mut(),
                0,
                0,
                frame_w.saturating_add(want_w),
                frame_h.saturating_add(want_h),
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
    Ok(hwnd)
}

impl Ui {
    fn emit(&self, event: WebViewEvent) {
        // The receiver lives until `close` has joined this thread, so this cannot fail while the
        // caller is still listening; after that nobody is.
        let _ = self.events.send(event);
    }

    /// Step 3: ask the runtime for an environment.
    fn start(&self, create: CreateEnvironment, user_data: &[u16]) {
        let me = self.me.clone();
        let handler = HandlerRef::completed(IID_EnvironmentCompleted, move |code, environment| {
            if let Some(ui) = me.upgrade() {
                ui.on_environment(code, environment);
            }
        });
        // SAFETY: this thread is an STA that pumps; the folder is NUL-terminated; the handler is
        // live, and WebView2 `AddRef`s it for as long as it needs it.
        let hr = unsafe { loader::create_environment(create, user_data, handler.as_raw()) };
        if hr < 0 {
            self.fail_start(format!(
                "CreateWebViewEnvironmentWithOptionsInternal failed with HRESULT {hr:#010x}"
            ));
        }
    }

    fn on_environment(&self, code: HRESULT, raw: *mut c_void) {
        if self.torn_down.get() {
            return;
        }
        // SAFETY: WebView2 lends the environment (or null) for the length of `Invoke`.
        let environment = unsafe { Com::<EnvironmentVtbl>::from_borrowed(raw) };
        let environment = match (code, environment) {
            (0.., Some(environment)) => environment,
            (code, _) => {
                return self.fail_start(format!(
                    "creating the WebView2 environment completed with HRESULT {code:#010x}"
                ));
            }
        };
        *self.environment.borrow_mut() = Some(environment.clone());
        let me = self.me.clone();
        let handler = HandlerRef::completed(IID_ControllerCompleted, move |code, controller| {
            if let Some(ui) = me.upgrade() {
                ui.on_controller(code, controller);
            }
        });
        // SAFETY: a live environment, this thread's live window, and a live handler.
        let hr = unsafe {
            (environment.vtbl().create_controller)(environment.as_raw(), self.hwnd.get(), handler.as_raw())
        };
        if hr < 0 {
            self.fail_start(format!(
                "ICoreWebView2Environment::CreateCoreWebView2Controller failed with HRESULT {hr:#010x}"
            ));
        }
    }

    fn on_controller(&self, code: HRESULT, raw: *mut c_void) {
        if self.torn_down.get() {
            return;
        }
        // SAFETY: WebView2 lends the controller (or null) for the length of `Invoke`.
        let controller = unsafe { Com::<ControllerVtbl>::from_borrowed(raw) };
        let controller = match (code, controller) {
            (0.., Some(controller)) => controller,
            (code, _) => {
                return self.fail_start(format!(
                    "creating the WebView2 controller completed with HRESULT {code:#010x}"
                ));
            }
        };
        let mut raw_webview: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live controller and an out-pointer live for the call.
        let hr = unsafe { (controller.vtbl().get_core_webview2)(controller.as_raw(), &raw mut raw_webview) };
        // SAFETY: a successful `get_CoreWebView2` hands over one reference to an `ICoreWebView2`.
        let webview = match (hr, unsafe { Com::<CoreWebView2Vtbl>::from_raw(raw_webview) }) {
            (0.., Some(webview)) => webview,
            (hr, _) => {
                return self.fail_start(format!(
                    "ICoreWebView2Controller::get_CoreWebView2 failed with HRESULT {hr:#010x}"
                ));
            }
        };
        *self.controller.borrow_mut() = Some(controller.clone());
        *self.webview.borrow_mut() = Some(webview.clone());

        self.fit();
        // SAFETY: a live controller.
        let hr = unsafe { (controller.vtbl().put_is_visible)(controller.as_raw(), 1) };
        if hr < 0 {
            return self.fail_start(format!("ICoreWebView2Controller::put_IsVisible failed with HRESULT {hr:#010x}"));
        }
        if let Err(message) = self.apply_user_agent(&webview) {
            return self.fail_start(message);
        }
        if let Err(message) = self.register_events(&webview) {
            return self.fail_start(message);
        }
        // SAFETY: no arguments.
        if unsafe { GetActiveWindow() } == self.hwnd.get() {
            self.move_focus();
        }

        let Some(script) = self.init_script.clone() else {
            return self.begin(webview);
        };
        let me = self.me.clone();
        let handler = HandlerRef::completed(IID_AddScriptCompleted, move |code, _id| {
            if let Some(ui) = me.upgrade() {
                ui.on_script_added(code);
            }
        });
        let script = wide(&script);
        // SAFETY: a live web view, a NUL-terminated script live for the call, a live handler.
        let hr = unsafe {
            (webview.vtbl().add_script_to_execute_on_document_created)(
                webview.as_raw(),
                script.as_ptr(),
                handler.as_raw(),
            )
        };
        if hr < 0 {
            self.fail_start(format!(
                "ICoreWebView2::AddScriptToExecuteOnDocumentCreated failed with HRESULT {hr:#010x}"
            ));
        }
    }

    fn on_script_added(&self, code: HRESULT) {
        if self.torn_down.get() {
            return;
        }
        if code < 0 {
            return self.fail_start(format!(
                "ICoreWebView2::AddScriptToExecuteOnDocumentCreated completed with HRESULT {code:#010x}"
            ));
        }
        let webview = self.webview.borrow().clone();
        match webview {
            Some(webview) => self.begin(webview),
            None => self.fail_start("the web view was released before its init script was added".into()),
        }
    }

    /// Ready: announce it, issue the first navigation, run what waited.
    fn begin(&self, webview: Com<CoreWebView2Vtbl>) {
        *self.phase.borrow_mut() = Phase::Ready(webview.clone());
        self.emit(WebViewEvent::Ready);
        let url = self.url.clone();
        self.run_queued(&webview, Queued::Navigate(url));
        let pending = core::mem::take(&mut *self.pending.borrow_mut());
        for queued in pending {
            self.run_queued(&webview, queued);
        }
    }

    /// The browser will never be ready: say why, and fail whatever was waiting for it.
    fn fail_start(&self, why: String) {
        *self.phase.borrow_mut() = Phase::Broken(why.clone());
        self.emit(WebViewEvent::Failed(why.clone()));
        let pending = core::mem::take(&mut *self.pending.borrow_mut());
        for queued in pending {
            self.emit(WebViewEvent::Failed(format!("{} not run: {why}", queued.operation())));
        }
    }

    /// `WebViewOptions::user_agent`, through `ICoreWebView2Settings2`, before anything navigates.
    /// A runtime without that interface is a failure naming it, never a silent default.
    fn apply_user_agent(&self, webview: &Com<CoreWebView2Vtbl>) -> Result<(), String> {
        let Some(agent) = &self.user_agent else {
            return Ok(());
        };
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live web view and an out-pointer live for the call.
        let hr = unsafe { (webview.vtbl().get_settings)(webview.as_raw(), &raw mut raw) };
        // SAFETY: a successful `get_Settings` hands over one reference to an
        // `ICoreWebView2Settings`, which begins with `IUnknown`'s slots.
        let settings = match (hr, unsafe { Com::<IUnknownVtbl>::from_raw(raw) }) {
            (0.., Some(settings)) => settings,
            (hr, _) => return Err(format!("ICoreWebView2::get_Settings failed with HRESULT {hr:#010x}")),
        };
        let settings2 = settings.query::<Settings2Vtbl>(&IID_Settings2).map_err(|hr| {
            format!(
                "this WebView2 runtime does not implement ICoreWebView2Settings2 \
                 ({{ee9a0f68-f46c-4e32-ac23-ef8cac224d2a}}; QueryInterface answered HRESULT \
                 {hr:#010x}), so the requested User-Agent cannot be set and nothing was navigated"
            )
        })?;
        let agent = wide(agent);
        // SAFETY: a live settings object and a NUL-terminated value live for the call.
        let hr = unsafe { (settings2.vtbl().put_user_agent)(settings2.as_raw(), agent.as_ptr()) };
        if hr < 0 {
            return Err(format!("ICoreWebView2Settings2::put_UserAgent failed with HRESULT {hr:#010x}"));
        }
        Ok(())
    }

    fn register_events(&self, webview: &Com<CoreWebView2Vtbl>) -> Result<(), String> {
        let vtbl = webview.vtbl();
        let me = self.me.clone();
        let starting = HandlerRef::event(IID_NavigationStartingHandler, move |args| {
            if let Some(ui) = me.upgrade() {
                ui.on_navigation_starting(args);
            }
        });
        self.register(webview, vtbl.add_navigation_starting, vtbl.remove_navigation_starting, &starting, "add_NavigationStarting")?;

        let me = self.me.clone();
        let completed = HandlerRef::event(IID_NavigationCompletedHandler, move |args| {
            if let Some(ui) = me.upgrade() {
                ui.on_navigation_completed(args);
            }
        });
        self.register(webview, vtbl.add_navigation_completed, vtbl.remove_navigation_completed, &completed, "add_NavigationCompleted")?;

        let me = self.me.clone();
        let message = HandlerRef::event(IID_WebMessageReceivedHandler, move |args| {
            if let Some(ui) = me.upgrade() {
                ui.on_web_message(args);
            }
        });
        self.register(webview, vtbl.add_web_message_received, vtbl.remove_web_message_received, &message, "add_WebMessageReceived")?;

        let me = self.me.clone();
        let failed = HandlerRef::event(IID_ProcessFailedHandler, move |args| {
            if let Some(ui) = me.upgrade() {
                ui.on_process_failed(args);
            }
        });
        self.register(webview, vtbl.add_process_failed, vtbl.remove_process_failed, &failed, "add_ProcessFailed")
    }

    fn register(
        &self,
        webview: &Com<CoreWebView2Vtbl>,
        add: AddHandler,
        remove: RemoveHandler,
        handler: &HandlerRef,
        api: &'static str,
    ) -> Result<(), String> {
        let mut token = EventRegistrationToken::default();
        // SAFETY: a live web view, a live handler of the interface this slot takes, and an
        // out-pointer live for the call.
        let hr = unsafe { add(webview.as_raw(), handler.as_raw(), &raw mut token) };
        if hr < 0 {
            return Err(format!("ICoreWebView2::{api} failed with HRESULT {hr:#010x}"));
        }
        self.registrations.borrow_mut().push((remove, token));
        Ok(())
    }

    fn on_navigation_starting(&self, raw: *mut c_void) {
        // SAFETY: WebView2 lends the event args for the length of `Invoke`.
        let Some(args) = (unsafe { Com::<NavigationStartingArgsVtbl>::from_borrowed(raw) }) else {
            return self.emit(WebViewEvent::Failed("NavigationStarting delivered no event args".into()));
        };
        // SAFETY: live args; `get_Uri` is the slot `co_string` is handed.
        let url = unsafe { co_string(args.as_raw(), args.vtbl().get_uri) };
        let mut id = 0u64;
        // SAFETY: live args and an out-pointer live for the call.
        let id_hr = unsafe { (args.vtbl().get_navigation_id)(args.as_raw(), &raw mut id) };
        match url {
            Ok(url) => {
                if id_hr >= 0 {
                    self.navigations.borrow_mut().insert(id, url.clone());
                }
                self.emit(WebViewEvent::NavigationStarting { url });
            }
            Err(code) => self.emit(WebViewEvent::Failed(format!(
                "NavigationStarting: get_Uri failed with HRESULT {code:#010x}"
            ))),
        }
    }

    fn on_navigation_completed(&self, raw: *mut c_void) {
        // SAFETY: WebView2 lends the event args for the length of `Invoke`.
        let Some(args) = (unsafe { Com::<NavigationCompletedArgsVtbl>::from_borrowed(raw) }) else {
            return self.emit(WebViewEvent::Failed("NavigationCompleted delivered no event args".into()));
        };
        let mut success = 0;
        // SAFETY: live args and an out-pointer live for the call.
        let success_hr = unsafe { (args.vtbl().get_is_success)(args.as_raw(), &raw mut success) };
        let mut id = 0u64;
        // SAFETY: as above.
        let id_hr = unsafe { (args.vtbl().get_navigation_id)(args.as_raw(), &raw mut id) };
        if success_hr < 0 {
            return self.emit(WebViewEvent::Failed(format!(
                "NavigationCompleted: get_IsSuccess failed with HRESULT {success_hr:#010x}"
            )));
        }
        let announced = if id_hr >= 0 { self.navigations.borrow_mut().remove(&id) } else { None };
        // Cloned out in its own statement, so the `RefCell` guard is gone before `get_Source`.
        let webview = self.webview.borrow().clone();
        let url = match (announced, webview) {
            (Some(url), _) => url,
            (None, Some(webview)) => {
                // SAFETY: a live web view; `get_Source` is the slot `co_string` is handed.
                let source = unsafe { co_string(webview.as_raw(), webview.vtbl().get_source) };
                source.unwrap_or_default()
            }
            (None, None) => String::new(),
        };
        self.emit(WebViewEvent::NavigationCompleted { url, success: success != 0 });
    }

    fn on_web_message(&self, raw: *mut c_void) {
        // SAFETY: WebView2 lends the event args for the length of `Invoke`.
        let Some(args) = (unsafe { Com::<WebMessageArgsVtbl>::from_borrowed(raw) }) else {
            return self.emit(WebViewEvent::Failed("WebMessageReceived delivered no event args".into()));
        };
        // SAFETY: live args; `TryGetWebMessageAsString` is the slot `co_string` is handed.
        let string = unsafe { co_string(args.as_raw(), args.vtbl().try_get_web_message_as_string) };
        let json = || {
            // SAFETY: live args; `get_WebMessageAsJson` is the slot `co_string` is handed.
            unsafe { co_string(args.as_raw(), args.vtbl().get_web_message_as_json) }
        };
        self.emit(message_event(string, json));
    }

    fn on_process_failed(&self, raw: *mut c_void) {
        // SAFETY: WebView2 lends the event args for the length of `Invoke`.
        let Some(args) = (unsafe { Com::<ProcessFailedArgsVtbl>::from_borrowed(raw) }) else {
            return self.emit(WebViewEvent::Failed("ProcessFailed delivered no event args".into()));
        };
        let mut kind = -1;
        // SAFETY: live args and an out-pointer live for the call.
        let hr = unsafe { (args.vtbl().get_process_failed_kind)(args.as_raw(), &raw mut kind) };
        if hr < 0 {
            return self.emit(WebViewEvent::Failed(format!(
                "ProcessFailed: get_ProcessFailedKind failed with HRESULT {hr:#010x}"
            )));
        }
        if let Some(event) = process_failed_event(kind) {
            self.emit(event);
        }
    }

    /// `WM_WAKE`: read every command that has arrived.
    fn drain_commands(&self) {
        while !self.torn_down.get() {
            let Ok(command) = self.commands.try_recv() else {
                return;
            };
            let queued = match command {
                Command::Close => {
                    self.closed_by_caller.set(true);
                    return self.teardown();
                }
                Command::Navigate(url) => Queued::Navigate(url),
                Command::ExecuteScript(script) => Queued::ExecuteScript(script),
            };
            let ready = match &*self.phase.borrow() {
                Phase::Starting => None,
                Phase::Ready(webview) => Some(Ok(webview.clone())),
                Phase::Broken(why) => Some(Err(why.clone())),
            };
            match ready {
                None => self.pending.borrow_mut().push_back(queued),
                Some(Ok(webview)) => self.run_queued(&webview, queued),
                Some(Err(why)) => self.emit(WebViewEvent::Failed(format!(
                    "{} not run: {why}",
                    queued.operation()
                ))),
            }
        }
    }

    fn run_queued(&self, webview: &Com<CoreWebView2Vtbl>, queued: Queued) {
        match queued {
            Queued::Navigate(url) => {
                let wide_url = wide(&url);
                // SAFETY: a live web view and a NUL-terminated URL live for the call.
                let hr = unsafe { (webview.vtbl().navigate)(webview.as_raw(), wide_url.as_ptr()) };
                if hr < 0 {
                    self.emit(WebViewEvent::Failed(format!(
                        "ICoreWebView2::Navigate({url}) failed with HRESULT {hr:#010x}"
                    )));
                }
            }
            Queued::ExecuteScript(script) => {
                let me = self.me.clone();
                let handler = HandlerRef::completed(IID_ExecuteScriptCompleted, move |code, _json| {
                    if code < 0 {
                        if let Some(ui) = me.upgrade() {
                            ui.emit(WebViewEvent::Failed(format!(
                                "ICoreWebView2::ExecuteScript completed with HRESULT {code:#010x}"
                            )));
                        }
                    }
                });
                let script = wide(&script);
                // SAFETY: a live web view, a NUL-terminated script live for the call, a live
                // handler.
                let hr = unsafe {
                    (webview.vtbl().execute_script)(webview.as_raw(), script.as_ptr(), handler.as_raw())
                };
                if hr < 0 {
                    self.emit(WebViewEvent::Failed(format!(
                        "ICoreWebView2::ExecuteScript failed with HRESULT {hr:#010x}"
                    )));
                }
            }
        }
    }

    /// Size the browser to the client area. Before the controller exists there is nothing to size.
    fn fit(&self) {
        let controller = self.controller.borrow().clone();
        let Some(controller) = controller else {
            return;
        };
        let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: a live window and a writable rectangle.
        if unsafe { GetClientRect(self.hwnd.get(), &raw mut client) } != 0 {
            // SAFETY: a live controller; `RECT` by value, as the header declares it.
            unsafe { (controller.vtbl().put_bounds)(controller.as_raw(), client) };
        }
    }

    /// Hand keyboard focus to the browser. Before the controller exists there is nowhere to hand it.
    fn move_focus(&self) {
        let controller = self.controller.borrow().clone();
        if let Some(controller) = controller {
            // SAFETY: a live controller and a constant reason.
            unsafe { (controller.vtbl().move_focus)(controller.as_raw(), MOVE_FOCUS_REASON_PROGRAMMATIC) };
        }
    }

    /// Release the browser and destroy the window: once, whoever asked.
    fn teardown(&self) {
        if self.torn_down.replace(true) {
            return;
        }
        // First, so that no command can be posted to a handle that is about to be destroyed.
        *lock(&self.wake) = None;
        *self.phase.borrow_mut() = Phase::Broken("the window has closed".into());
        let webview = self.webview.borrow_mut().take();
        let registrations = core::mem::take(&mut *self.registrations.borrow_mut());
        if let Some(webview) = &webview {
            for (remove, token) in registrations {
                // SAFETY: a live web view and a token its matching `add_` returned. The result is
                // ignored: there is nobody to report it to, and `Close` follows regardless.
                unsafe { remove(webview.as_raw(), token) };
            }
        }
        drop(webview);
        let controller = self.controller.borrow_mut().take();
        if let Some(controller) = &controller {
            // SAFETY: a live controller; `Close` releases the browser's hold on this window.
            unsafe { (controller.vtbl().close)(controller.as_raw()) };
        }
        drop(controller);
        drop(self.environment.borrow_mut().take());
        // SAFETY: this thread's live window. Sends `WM_DESTROY`, which posts the `WM_QUIT` that
        // ends the pump.
        unsafe { DestroyWindow(self.hwnd.get()) };
    }
}

/// The window procedure: the four messages that matter to a browser window, and the wake.
///
/// # Safety
///
/// Called by Win32 on this thread with a live `hwnd` whose `GWLP_USERDATA` is null or the `Ui` that
/// `run` keeps alive until after the window is destroyed.
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        // SAFETY: for `WM_NCCREATE`, `lparam` is a live `CREATESTRUCTW` whose `lpCreateParams` is
        // the `state` pointer `create_window` passed.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        // SAFETY: stores a pointer-sized value in the window's user-data slot.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
        // SAFETY: forwarding unchanged; `DefWindowProcW` must see `WM_NCCREATE`.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: reads the slot written above.
    let ui = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Ui;
    if ui.is_null() {
        // SAFETY: forwarding unchanged.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: non-null, so it is `run`'s live `Ui` (cleared at `WM_NCDESTROY`, the last message);
    // only shared references to it are ever made.
    let ui = unsafe { &*ui };
    match msg {
        WM_SIZE => {
            guarded(|| ui.fit());
        }
        WM_ACTIVATE => {
            // `DefWindowProcW` gives the activated window keyboard focus, so it runs first and the
            // browser takes the focus from it after.
            // SAFETY: forwarding unchanged.
            let result = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            if (wparam & 0xFFFF) as u32 != WA_INACTIVE {
                guarded(|| ui.move_focus());
            }
            return result;
        }
        WM_SETFOCUS => {
            guarded(|| ui.move_focus());
            return 0;
        }
        WM_CLOSE => {
            // The person closed it. Not `DefWindowProcW`'s bare `DestroyWindow`: the browser is
            // released first.
            guarded(|| ui.teardown());
            return 0;
        }
        WM_DESTROY => {
            // SAFETY: no arguments; ends this thread's pump.
            unsafe { PostQuitMessage(0) };
            return 0;
        }
        WM_NCDESTROY => {
            // SAFETY: as the write at `WM_NCCREATE`.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
        }
        WM_WAKE => {
            guarded(|| ui.drain_commands());
            return 0;
        }
        _ => {}
    }
    // SAFETY: forwarding unchanged.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::UI::WindowsAndMessaging::{SC_CLOSE, WM_SYSCOMMAND};

    use super::super::WebView as PublicWebView;
    use super::*;

    #[test]
    fn a_string_message_is_a_message_and_anything_else_is_named_as_not_one() {
        assert_eq!(message_event(Ok("hello".into()), || panic!("not asked")), WebViewEvent::Message("hello".into()));
        assert_eq!(
            message_event(Err(E_INVALIDARG), || Ok("{\"a\":1}".into())),
            WebViewEvent::NonStringMessage { json: "{\"a\":1}".into() }
        );
        match message_event(Err(E_INVALIDARG), || Err(E_FAIL)) {
            WebViewEvent::Failed(text) => assert!(text.contains("0x80004005"), "{text}"),
            other => panic!("{other:?}"),
        }
        match message_event(Err(E_FAIL), || panic!("only E_INVALIDARG means 'not a string'")) {
            WebViewEvent::Failed(text) => assert!(text.contains("TryGetWebMessageAsString"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    /// The kinds WebView2.idl says need handling are reported by name; the ones it says recover by
    /// themselves are not. Values from WebView2.h's `COREWEBVIEW2_PROCESS_FAILED_KIND`.
    #[test]
    fn process_failures_are_reported_when_the_page_is_affected() {
        for (kind, name) in [
            (0, "BROWSER_PROCESS_EXITED"),
            (1, "RENDER_PROCESS_EXITED"),
            (2, "RENDER_PROCESS_UNRESPONSIVE"),
            (3, "FRAME_RENDER_PROCESS_EXITED"),
            (9, "unknown"),
            (42, "unknown"),
        ] {
            match process_failed_event(kind) {
                Some(WebViewEvent::Failed(text)) => assert!(text.contains(name), "{kind}: {text}"),
                other => panic!("{kind}: {other:?}"),
            }
        }
        for kind in 4..=8 {
            assert_eq!(process_failed_event(kind), None, "kind {kind} recovers by itself");
        }
    }

    #[test]
    fn strings_are_nul_terminated_utf16_and_codes_are_hresults() {
        assert_eq!(wide("aé"), [0x61, 0xE9, 0]);
        assert_eq!(wide(""), [0]);
        assert_eq!(wide_os(OsStr::new("C:\\x")), [0x43, 0x3A, 0x5C, 0x78, 0]);
        assert_eq!(hresult_from_win32(203) as u32, 0x8007_00CB, "ERROR_ENVVAR_NOT_FOUND");
        assert_eq!(hresult_from_win32(0), 0);
    }

    /// **The title bar's X**, as Win32 delivers it (`WM_SYSCOMMAND` / `SC_CLOSE`, which
    /// `DefWindowProcW` turns into `WM_CLOSE`): `Closed` exactly once and last, the thread ends by
    /// itself, and every command after answers `Closed`. Needs the window handle, which only this
    /// module can see, so it is here rather than in `tests/webview_live.rs`; gated the same way.
    #[test]
    #[ignore = "opens a real window: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
    fn closing_from_the_title_bar_sends_closed_once_and_ends_the_thread() {
        assert!(
            std::env::var("OMNI_WEBVIEW_LIVE_TESTS").is_ok_and(|v| v == "1"),
            "run with --ignored but OMNI_WEBVIEW_LIVE_TESTS is not 1; this opens a real window"
        );
        let view = PublicWebView::open(&WebViewOptions {
            title: "omnidroid: title-bar close".into(),
            url: "data:text/html,<p>close me</p>".into(),
            width: 480,
            height: 320,
            init_script: None,
            user_agent: None,
        })
        .expect("open");
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !seen.iter().any(|e| matches!(e, WebViewEvent::NavigationCompleted { .. })) {
            assert!(Instant::now() < deadline, "no NavigationCompleted in 30 s: {seen:?}");
            seen.extend(view.poll_events());
            std::thread::sleep(Duration::from_millis(10));
        }

        let hwnd = lock(&view.inner.wake).expect("a live window") as HWND;
        // SAFETY: a live window of this process; no pointer arguments.
        assert_ne!(unsafe { PostMessageW(hwnd, WM_SYSCOMMAND, SC_CLOSE as usize, 0) }, 0);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let finished = lock(&view.inner.thread).as_ref().is_some_and(JoinHandle::is_finished);
            seen.extend(view.poll_events());
            if finished {
                break;
            }
            assert!(Instant::now() < deadline, "the thread did not end 10 s after SC_CLOSE: {seen:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        seen.extend(view.poll_events());
        let closed = seen.iter().filter(|e| **e == WebViewEvent::Closed).count();
        assert_eq!(closed, 1, "{seen:?}");
        assert_eq!(seen.last(), Some(&WebViewEvent::Closed), "Closed must be last: {seen:?}");
        assert_eq!(view.navigate("data:text/html,x"), Err(WebViewError::Closed { operation: "navigate" }));
        view.close();
        view.close();
        assert_eq!(view.poll_events(), [], "nothing after Closed");
    }
}
