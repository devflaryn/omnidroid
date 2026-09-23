//! Unix backend for the web-view seam, shared by Linux and macOS.
//!
//! # Status: structural, not implemented
//!
//! **Nothing in this module has ever been run.** [`WebView::open`] and [`runtime_version`] return
//! [`WebViewError::Unsupported`] naming the platform API they intend to reach for.
//!
//! `WebView` here is an **uninhabited** type, for the reason `window::unix` gives: `open` is the
//! only way to get one and it refuses, so every other operation's body is `match *self {}` — a
//! statement the compiler proves rather than a refusal no test could reach (VERIFICATION entry 12).
//!
//! # What implementing this involves
//!
//! * **Linux**: WebKitGTK — `webkit_web_view_new(3)` in a `GtkWindow`, with
//!   `webkit_user_content_manager_add_script` for the init script (it has an explicit
//!   all-frames/top-frame choice, which WebView2 does not) and a script message handler for
//!   `postMessage`. GTK wants its own main loop on one thread, which this seam's "a UI thread the
//!   seam owns" shape already provides.
//! * **macOS**: `WKWebView` with a `WKUserScript` and a `WKScriptMessageHandler`. AppKit requires
//!   the **main thread**, which collides with that same shape exactly as `window::unix` records
//!   for windows; the resolution is a change to the seam, not to this file.

use super::{Command, WebViewError, WebViewEvent, WebViewOptions, WebViewResult};

/// The intended implementation, for the refusal.
const INTENDED: &str = "webkit_web_view_new(3) (WebKitGTK) on Linux, WKWebView on macOS";

/// Unsupported: see this module's header.
pub(super) fn runtime_version() -> WebViewResult<String> {
    Err(WebViewError::Unsupported {
        operation: "runtime_version",
        intended: INTENDED,
        target: std::env::consts::OS,
    })
}

/// The structural unix web view: **a type with no values**.
pub(super) enum WebView {}

impl WebView {
    /// The one reachable operation, and it refuses.
    pub(super) fn open(options: &WebViewOptions) -> WebViewResult<Self> {
        let _ = options;
        Err(WebViewError::Unsupported { operation: "open", intended: INTENDED, target: std::env::consts::OS })
    }

    /// Unreachable: `open` never produces a `WebView`, so the compiler discharges this.
    pub(super) fn poll_events(&self) -> Vec<WebViewEvent> {
        match *self {}
    }

    /// Unreachable, as [`WebView::poll_events`].
    pub(super) fn send(&self, command: Command, operation: &'static str) -> WebViewResult<()> {
        let _ = (command, operation);
        match *self {}
    }

    /// Unreachable, as [`WebView::poll_events`].
    pub(super) fn close(&self) {
        match *self {}
    }
}
