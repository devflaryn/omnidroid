//! A host web view: a top-level window whose client area is a real browser.
//!
//! # What this module is
//!
//! The host side of Android's `WebView` for pages the guest cannot render itself — first of all
//! the Roblox sign-in captcha ("challenge") page, which the app opens in a web view of its own and
//! which this runtime otherwise has nowhere to show. `omni-android`'s `WebViewProtocol` emulation
//! drives it; nothing else in the workspace may reach a browser engine (Global Constraint 4).
//!
//! On Windows it is **Microsoft Edge WebView2**, the Chromium runtime Windows 11 ships, found and
//! loaded without the SDK's `WebView2Loader.dll` (see `windows/loader.rs`). The unix backend is
//! structural and refuses by name.
//!
//! The backend calls, fixed by the compiler as in the other seams:
//!
//! ```text
//! runtime_version() -> WebViewResult<String>
//! WebView::open(&WebViewOptions) -> WebViewResult<WebView>
//! WebView::poll_events(&self) -> Vec<WebViewEvent>
//! WebView::send(&self, Command, operation) -> WebViewResult<()>
//! WebView::close(&self)
//! ```
//!
//! # A thread the seam owns, and a caller that never pumps
//!
//! WebView2 must live on a single-threaded-apartment thread with a message pump, and every one of
//! its callbacks arrives through that pump. The caller here is a runtime thread with other work,
//! so [`WebView::open`] **starts a thread of its own** — COM, the window, the browser environment,
//! the controller and the pump all live there — and the rest of the API talks to it:
//!
//! * **Commands** ([`WebView::navigate`], [`WebView::execute_script`], [`WebView::close`]) go
//!   into a channel, and a posted window message wakes the pump to read it. `Ok` from a command
//!   means **queued**, not done: what the host said about it arrives later as an event.
//! * **Events** come back through a channel that [`WebView::poll_events`] drains without
//!   blocking. The window's own lifecycle — the person closing it — is an event like any other.
//!
//! `open` blocks only until the window exists (so a missing runtime, a bad argument or a failed
//! `CreateWindowExW` are its errors); the browser comes up asynchronously and announces itself
//! with [`WebViewEvent::Ready`]. Commands sent before that are held and run, in order, straight
//! after the first navigation is issued.
//!
//! # The init script, and what reaches iframes
//!
//! [`WebViewOptions::init_script`] is `AddScriptToExecuteOnDocumentCreated`, and the first
//! navigation is issued only from that call's **completion** — WebView2.idl: "you must wait for
//! the completion handler to finish before the injected script is ready to run". So the script is
//! in place before the first page's own scripts, and it runs again in every later document.
//!
//! What happens in **iframes** was measured rather than read, because the captcha provider may
//! put its UI in one (`tests/webview_live.rs`, `what_reaches_an_iframe`, with a same-origin
//! `srcdoc` frame and a cross-origin `data:` frame):
//!
//! * the init script **does** run in both frames, before their own scripts, and both have
//!   `window.chrome.webview`;
//! * but `window.chrome.webview.postMessage` **from inside a frame does not reach this seam** —
//!   none of four such posts arrived. WebView2.idl routes a frame's posts to
//!   `ICoreWebView2Frame2::add_WebMessageReceived`, not to `ICoreWebView2`'s.
//!
//! So an interface object the init script defines works in the top document (Roblox's own page,
//! which is where the app's JavaScript interface is called from) and is **present but mute** in a
//! frame. Reaching frames would take `ICoreWebView2_4::add_FrameCreated` (slot 73), the args'
//! `get_Frame`, a `QueryInterface` to `ICoreWebView2Frame2` and its `add_WebMessageReceived`
//! (slot 22) — about five more hand-declared pieces — and only for the top document's *direct*
//! children; a frame inside a frame needs `ICoreWebView2Frame7`'s own `FrameCreated`. Not done.
//!
//! # Messages
//!
//! `window.chrome.webview.postMessage(x)` in the top-level document arrives as
//! [`WebViewEvent::Message`] when `x` is a string (`TryGetWebMessageAsString`) and as
//! [`WebViewEvent::NonStringMessage`] with its JSON when it is anything else: never dropped.
//!
//! # Focus and size
//!
//! The window is an ordinary resizable top-level window (`WS_OVERLAPPEDWINDOW`), created at the
//! requested **client** size in physical pixels under per-monitor DPI awareness, as the window
//! seam does it. The browser fills its client area and is resized on every `WM_SIZE`. Keyboard
//! focus is handed into the browser with `ICoreWebView2Controller::MoveFocus` whenever the window
//! is activated or given focus, because the person types into the page.
//!
//! # Closing
//!
//! [`WebView::close`] (and `Drop`) asks the thread to release the browser, destroy the window and
//! stop, and **waits for it to end**. The person closing the window (the title bar's X, Alt+F4)
//! does the same teardown on the thread's own initiative and produces [`WebViewEvent::Closed`]
//! exactly once; after either, every command answers [`WebViewError::Closed`].
//!
//! # MEASURED on the development host
//!
//! Windows 11 x86-64, WebView2 runtime **153.0.4234.48** (per-machine, found through the `HKLM`
//! fallback), `tests/webview_live.rs` in release:
//!
//! * a `data:` page's `postMessage('hello')` arrived 430–650 ms after `open` was called (two runs,
//!   the browser process already warm or not), after `Ready` and `NavigationStarting` and before
//!   `NavigationCompleted`;
//! * `NavigationStarting` and `NavigationCompleted` report a `data:` URL byte-for-byte as it was
//!   passed; a navigation to a refused `127.0.0.1` port completes with `success: false`;
//! * an init-script object is callable from the page's first, parse-time script
//!   (`document.readyState == "loading"`), and again after a second navigation;
//! * `postMessage({a: 1})` arrives as `NonStringMessage { json: "{\"a\":1}" }`;
//! * a `User-Agent` set through `ICoreWebView2Settings2` is both `navigator.userAgent` and the
//!   `User-Agent` header of the request that fetched the page;
//! * `document.hasFocus()` was `true` at the page's `load` in the one run that printed it — the
//!   window was activated and `MoveFocus` handed focus in. Whether a window may take the
//!   foreground at all is the foreground lock's decision, not this seam's, so it is printed and
//!   not asserted;
//! * the title bar's close (`WM_SYSCOMMAND`/`SC_CLOSE`) produced exactly one `Closed`, last, and
//!   the thread ended on its own.

use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;

mod error;

pub use error::{WebViewError, WebViewResult};

// The backend modules are **private**, for the reason `vm::mod` records: a `pub mod windows` is a
// public surface no other crate can name without writing `#[cfg(target_os = "windows")]` itself.
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as backend;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as backend;

/// The largest client extent accepted in either axis: `WM_SIZE` reports the client size in two
/// 16-bit halves, the window seam's `MAX_EXTENT` reasoning.
const MAX_EXTENT: u32 = u16::MAX as u32;

/// What to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebViewOptions {
    /// The window's title.
    pub title: String,
    /// The first page. Anything WebView2's `Navigate` takes: `https://…`, `data:…`.
    pub url: String,
    /// The client area's width, in physical pixels. 1 to 65,535.
    pub width: u32,
    /// The client area's height, in physical pixels. 1 to 65,535.
    pub height: u32,
    /// Added with `AddScriptToExecuteOnDocumentCreated` before the first navigation is issued, so
    /// it runs before any script of the first page and of every page after it. See this module's
    /// "The init script, and what reaches iframes".
    pub init_script: Option<String>,
    /// Replaces the browser's User-Agent for every request and `navigator.userAgent`
    /// (`ICoreWebView2Settings2::put_UserAgent`). `None` keeps WebView2's default.
    ///
    /// Applied before the first navigation. **Added at the lead's request** after the brief: the
    /// Roblox app sets its own User-Agent on every web view and Roblox's pages switch on the
    /// in-app bridge from it. A runtime without `ICoreWebView2Settings2` cannot honour it, and
    /// then the browser never navigates: [`WebViewEvent::Failed`] names the interface. That
    /// arrives as an event rather than as `open`'s error because the settings object exists only
    /// once the controller has been created, asynchronously, after `open` returned. `Some("")` is
    /// refused by `open`: WebView2.idl says an empty value leaves the User-Agent unchanged, which
    /// would be a request silently not honoured.
    pub user_agent: Option<String>,
}

/// Something that happened in the web view, in the order it happened.
#[derive(Debug, Clone, PartialEq)]
pub enum WebViewEvent {
    /// The window and the browser are ready, and the first navigation is being issued. Every
    /// navigation event follows it; commands sent before it run straight after it.
    Ready,
    /// A top-level navigation started (and again for each redirect), with the URL as the browser
    /// reports it.
    NavigationStarting {
        /// The URL being navigated to.
        url: String,
    },
    /// A top-level navigation finished. `url` is the last URL its `NavigationStarting` named —
    /// the one redirects ended at — or, for a navigation that never announced itself, the
    /// browser's current source.
    NavigationCompleted {
        /// The URL that was navigated to.
        url: String,
        /// `ICoreWebView2NavigationCompletedEventArgs::IsSuccess`: false for a network error, an
        /// HTTP error page the browser substituted, or a cancelled navigation.
        success: bool,
    },
    /// The top-level document ran `window.chrome.webview.postMessage(string)`.
    Message(String),
    /// The top-level document posted something that is **not** a string; `json` is it, as
    /// `get_WebMessageAsJson` renders it. **Not in the brief's API; added** so that a non-string
    /// post is reported rather than dropped or passed off as a string.
    NonStringMessage {
        /// The posted value as JSON.
        json: String,
    },
    /// The person closed the window. Sent once, and last. **Not** sent for
    /// [`WebView::close`], which the caller already knows about.
    Closed,
    /// Something failed after `open` returned, named with the host call and its `HRESULT`: the
    /// browser environment or controller could not be created, a browser or page process died
    /// or hung, or the host refused a queued command.
    Failed(String),
}

/// A command for the web view's thread.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Navigate(String),
    ExecuteScript(String),
    Close,
}

/// A top-level host window with a web view filling its client area.
///
/// # Thread affinity
///
/// `Send`: it is opened by one runtime thread and may be driven and dropped by another; all it
/// holds is channels to the web view's own thread and that thread's handle. Not `Sync` — the event
/// receiver is single-consumer.
pub struct WebView {
    inner: backend::WebView,
    /// `Cell` is `Send` and not `Sync`, the property wanted; here rather than left to the backend
    /// so it holds on every target (the unix backend's type is uninhabited, and would be `Sync`).
    _not_sync: PhantomData<Cell<()>>,
}

impl WebView {
    /// The installed browser runtime's version, e.g. `"153.0.4234.48"`. Never opens a window or
    /// loads the runtime.
    ///
    /// # Errors
    ///
    /// [`WebViewError::RuntimeMissing`] when no usable runtime is installed; [`WebViewError::Os`]
    /// when the registry could not be read; [`WebViewError::Unsupported`] on unix.
    pub fn runtime_version() -> WebViewResult<String> {
        backend::runtime_version()
    }

    /// Open the window and start the browser in it. Returns once the window exists; the browser
    /// arrives asynchronously, announced by [`WebViewEvent::Ready`].
    ///
    /// # Errors
    ///
    /// [`WebViewError::InvalidArgument`] for a NUL in any string, an empty URL, or a size outside
    /// 1..=65535; [`WebViewError::RuntimeMissing`]; [`WebViewError::Os`] when the thread, COM or
    /// the window could not be set up; [`WebViewError::Unsupported`] on unix.
    pub fn open(options: &WebViewOptions) -> WebViewResult<WebView> {
        validate(options)?;
        Ok(WebView { inner: backend::WebView::open(options)?, _not_sync: PhantomData })
    }

    /// Everything that happened since the last call, oldest first. Never blocks; empty when
    /// nothing did.
    #[must_use]
    pub fn poll_events(&self) -> Vec<WebViewEvent> {
        self.inner.poll_events()
    }

    /// Run `script` in the current top-level document. `Ok` means queued; a refusal by the host
    /// arrives as [`WebViewEvent::Failed`]. The script's own result is not reported: to get a value
    /// out, `postMessage` it.
    ///
    /// # Errors
    ///
    /// [`WebViewError::Closed`] after the window has closed; [`WebViewError::InvalidArgument`] for
    /// a NUL in the script.
    pub fn execute_script(&self, script: &str) -> WebViewResult<()> {
        no_nul("execute_script", "script", script)?;
        self.inner.send(Command::ExecuteScript(script.to_owned()), "execute_script")
    }

    /// Navigate the top-level document to `url`. `Ok` means queued; the outcome arrives as
    /// [`WebViewEvent::NavigationCompleted`], or [`WebViewEvent::Failed`] if the host refused the
    /// URL outright.
    ///
    /// # Errors
    ///
    /// [`WebViewError::Closed`] after the window has closed; [`WebViewError::InvalidArgument`] for
    /// an empty URL or a NUL in it.
    pub fn navigate(&self, url: &str) -> WebViewResult<()> {
        validate_url("navigate", url)?;
        self.inner.send(Command::Navigate(url.to_owned()), "navigate")
    }

    /// Close the window, release the browser and end the web view's thread, waiting for it.
    /// Idempotent, and a no-op after the person already closed the window. `Drop` calls it.
    pub fn close(&self) {
        self.inner.close();
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        self.inner.close();
    }
}

impl fmt::Debug for WebView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebView").finish_non_exhaustive()
    }
}

/// Refuse a string WebView2 would truncate at an interior NUL.
fn no_nul(operation: &'static str, argument: &'static str, value: &str) -> WebViewResult<()> {
    match value.chars().position(|c| c == '\0') {
        None => Ok(()),
        Some(at) => Err(WebViewError::InvalidArgument {
            operation,
            argument,
            detail: format!(
                "contains a NUL character at index {at} (in chars); the host takes NUL-terminated \
                 strings and would truncate it there"
            ),
        }),
    }
}

fn validate_url(operation: &'static str, url: &str) -> WebViewResult<()> {
    if url.is_empty() {
        return Err(WebViewError::InvalidArgument { operation, argument: "url", detail: "is empty".into() });
    }
    no_nul(operation, "url", url)
}

/// Everything `open` refuses before a host call.
fn validate(options: &WebViewOptions) -> WebViewResult<()> {
    no_nul("open", "title", &options.title)?;
    validate_url("open", &options.url)?;
    if let Some(script) = &options.init_script {
        no_nul("open", "init_script", script)?;
    }
    if let Some(agent) = &options.user_agent {
        if agent.is_empty() {
            return Err(WebViewError::InvalidArgument {
                operation: "open",
                argument: "user_agent",
                detail: "is empty; WebView2 leaves the User-Agent unchanged for an empty value, so \
                         the request would be silently ignored (use None for the default)"
                    .into(),
            });
        }
        no_nul("open", "user_agent", agent)?;
    }
    for (argument, value) in [("width", options.width), ("height", options.height)] {
        if value == 0 || value > MAX_EXTENT {
            return Err(WebViewError::InvalidArgument {
                operation: "open",
                argument,
                detail: format!("is {value}; it must be 1 to {MAX_EXTENT} physical pixels"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> WebViewOptions {
        WebViewOptions {
            title: "t".into(),
            url: "https://example.com/".into(),
            width: 640,
            height: 480,
            init_script: Some("window.x = 1;".into()),
            user_agent: Some("Mozilla/5.0 ROBLOX Android App 2.738.1397 Phone Hybrid()".into()),
        }
    }

    #[test]
    fn a_usable_request_passes_validation() {
        assert_eq!(validate(&options()), Ok(()));
        let edges = WebViewOptions {
            width: 1,
            height: MAX_EXTENT,
            title: String::new(),
            init_script: None,
            user_agent: None,
            ..options()
        };
        assert_eq!(validate(&edges), Ok(()), "an empty title and the extreme sizes are fine");
    }

    #[test]
    fn a_nul_anywhere_is_refused_naming_the_argument_and_the_position() {
        let title = WebViewOptions { title: "ab\0c".into(), ..options() };
        match validate(&title) {
            Err(WebViewError::InvalidArgument { operation: "open", argument: "title", detail }) => {
                assert!(detail.contains("index 2"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
        let url = WebViewOptions { url: "https://a\0".into(), ..options() };
        assert!(matches!(validate(&url), Err(WebViewError::InvalidArgument { argument: "url", .. })));
        let script = WebViewOptions { init_script: Some("\0".into()), ..options() };
        assert!(matches!(validate(&script), Err(WebViewError::InvalidArgument { argument: "init_script", .. })));
        let agent = WebViewOptions { user_agent: Some("UA\0".into()), ..options() };
        assert!(matches!(validate(&agent), Err(WebViewError::InvalidArgument { argument: "user_agent", .. })));
        assert!(matches!(
            no_nul("execute_script", "script", "a\0"),
            Err(WebViewError::InvalidArgument { operation: "execute_script", argument: "script", .. })
        ));
        assert!(matches!(
            validate_url("navigate", "x\0"),
            Err(WebViewError::InvalidArgument { operation: "navigate", argument: "url", .. })
        ));
    }

    /// WebView2 ignores an empty User-Agent (WebView2.idl, `put_UserAgent`), so asking for one is
    /// refused rather than silently not honoured.
    #[test]
    fn an_empty_user_agent_is_refused() {
        let empty = WebViewOptions { user_agent: Some(String::new()), ..options() };
        match validate(&empty) {
            Err(WebViewError::InvalidArgument { argument: "user_agent", detail, .. }) => {
                assert!(detail.contains("empty"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_empty_url_and_an_unmakeable_size_are_refused() {
        let empty = WebViewOptions { url: String::new(), ..options() };
        assert!(matches!(validate(&empty), Err(WebViewError::InvalidArgument { argument: "url", .. })));
        for (width, height, argument) in [(0, 480, "width"), (640, 0, "height"), (MAX_EXTENT + 1, 480, "width")] {
            let bad = WebViewOptions { width, height, ..options() };
            match validate(&bad) {
                Err(WebViewError::InvalidArgument { argument: got, detail, .. }) => {
                    assert_eq!(got, argument);
                    assert!(detail.contains(&(if argument == "width" { width } else { height }).to_string()), "{detail}");
                }
                other => panic!("{width}x{height}: {other:?}"),
            }
        }
    }

    /// `Send`, and not `Sync`, checked by the compiler on every target (the audio seam's trick).
    #[test]
    fn web_view_is_send_and_not_sync() {
        fn assert_send<T: Send>() {}
        assert_send::<WebView>();

        trait AmbiguousIfSync<A> {
            fn some_item() {}
        }
        impl<T: ?Sized> AmbiguousIfSync<()> for T {}
        impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}
        <WebView as AmbiguousIfSync<_>>::some_item();
    }
}
