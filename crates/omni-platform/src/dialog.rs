//! **A question for the person, on the desktop**: a small native dialog with two buttons, for a
//! program with no window of its own to ask from. Windows: `MessageBoxW` with its buttons renamed.
//! macOS: `osascript`. Linux: `zenity`, else `kdialog`.

/// Ask `text` under `title`, with two buttons: `Some(true)` for `first` (the default), `Some(false)`
/// for `second`, `None` when no dialog could be shown (no desktop, no dialog program).
#[must_use]
pub fn ask(title: &str, text: &str, first: &str, second: &str) -> Option<bool> {
    backend::ask(title, text, first, second)
}

#[cfg(windows)]
mod backend {
    use std::cell::RefCell;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    use windows_sys::Win32::UI::WindowsAndMessaging::{CallNextHookEx, MessageBoxW, SetDlgItemTextW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK};

    const WH_CBT: i32 = 5;
    const HCBT_ACTIVATE: i32 = 5;
    const IDYES: i32 = 6;
    const IDNO: i32 = 7;
    /// `MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST`: no cancel, so the close box is
    /// off too -- and over the desktop, since the window it is about has just gone.
    const STYLE: u32 = 0x4 | 0x20 | 0x1_0000 | 0x4_0000;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    thread_local! {
        /// The buttons' names, and the hook that gives them, for the dialog on this thread.
        static RENAME: RefCell<Option<(HHOOK, Vec<u16>, Vec<u16>)>> = const { RefCell::new(None) };
    }

    /// The moment the message box activates: its Yes and No named, and the hook gone.
    unsafe extern "system" fn rename(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HCBT_ACTIVATE {
            RENAME.with(|r| {
                if let Some((hook, first, second)) = r.borrow_mut().take() {
                    let dialog = wparam as HWND;
                    // SAFETY: the dialog this hook is told is activating, and NUL-ended names that
                    // live across the calls; the hook is this thread's own.
                    unsafe {
                        SetDlgItemTextW(dialog, IDYES, first.as_ptr());
                        SetDlgItemTextW(dialog, IDNO, second.as_ptr());
                        UnhookWindowsHookEx(hook);
                    }
                }
            });
        }
        // SAFETY: as every hook passes on what it was given; a null hook is allowed.
        unsafe { CallNextHookEx(core::ptr::null_mut(), code, wparam, lparam) }
    }

    pub(super) fn ask(title: &str, text: &str, first: &str, second: &str) -> Option<bool> {
        // SAFETY: by-value constant; a refusal (the awareness set already) changes nothing here.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        // SAFETY: a hook procedure of this module, on this thread alone, removed by itself or below.
        let hook = unsafe { SetWindowsHookExW(WH_CBT, Some(rename), core::ptr::null_mut(), GetCurrentThreadId()) };
        if !hook.is_null() {
            RENAME.with(|r| *r.borrow_mut() = Some((hook, wide(first), wide(second))));
        }
        let (text, title) = (wide(text), wide(title));
        // SAFETY: NUL-ended strings that live across the call; no owner window.
        let answer = unsafe { MessageBoxW(core::ptr::null_mut(), text.as_ptr(), title.as_ptr(), STYLE) };
        // Not taken if the box never activated.
        if let Some((hook, _, _)) = RENAME.with(|r| r.borrow_mut().take()) {
            // SAFETY: this thread's own hook, not yet removed.
            unsafe { UnhookWindowsHookEx(hook) };
        }
        match answer {
            IDYES => Some(true),
            IDNO => Some(false),
            _ => None,
        }
    }
}

#[cfg(target_os = "macos")]
mod backend {
    use std::process::{Command, Stdio};

    /// Text as an AppleScript string.
    fn quoted(text: &str) -> String {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }

    pub(super) fn ask(title: &str, text: &str, first: &str, second: &str) -> Option<bool> {
        let script = format!(
            "display dialog {} with title {} buttons {{{}, {}}} default button {}",
            quoted(text),
            quoted(title),
            quoted(second),
            quoted(first),
            quoted(first)
        );
        let out = Command::new("/usr/bin/osascript").args(["-e", &script]).stdin(Stdio::null()).output().ok()?;
        let said = String::from_utf8_lossy(&out.stdout);
        let button = said.trim().strip_prefix("button returned:")?;
        Some(button == first)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod backend {
    use std::process::{Command, Stdio};

    /// A dialog program's answer: `Some` for its yes (0) or no (1), `None` when it could not run.
    fn answer(program: &str, args: &[String]) -> Option<bool> {
        match Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().ok()?.code()? {
            0 => Some(true),
            1 => Some(false),
            _ => None,
        }
    }

    pub(super) fn ask(title: &str, text: &str, first: &str, second: &str) -> Option<bool> {
        let zenity = [format!("--title={title}"), format!("--text={text}"), format!("--ok-label={first}"), format!("--cancel-label={second}"), "--no-markup".to_string(), "--question".to_string()];
        let kdialog = ["--title".to_string(), title.to_string(), format!("--yes-label={first}"), format!("--no-label={second}"), "--yesno".to_string(), text.to_string()];
        answer("zenity", &zenity).or_else(|| answer("kdialog", &kdialog))
    }
}
