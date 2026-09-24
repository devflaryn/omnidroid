//! macOS backend for the web-view seam: **`WKWebView`** in an `NSWindow`, on the AppKit thread the
//! window seam owns.
//!
//! # The thread
//!
//! `WKWebView` is main-thread-only, as AppKit is. The window seam already hands the process's main
//! thread to AppKit before `main` and serves work sent to it (`window::macos::main_thread`); this
//! backend uses that thread rather than starting one, which is the one difference from the
//! WebView2 backend's shape ("a thread the seam owns"). Every call reaches it through
//! `appkit_thread::on_main`, synchronously; the `WKWebView`, its window and its delegate live in a
//! registry that only the main thread touches, and the [`WebView`] handle the caller holds is an id
//! plus the event channel, so it is `Send` as the seam requires. Without an AppKit thread -- a
//! process whose `main` the window seam could not move -- `open` refuses with
//! [`WebViewError::MainThreadUnavailable`], naming why.
//!
//! # The page's `postMessage`
//!
//! The seam's contract is WebView2's: the top-level document's
//! `window.chrome.webview.postMessage(x)` arrives as [`WebViewEvent::Message`] for a string and as
//! [`WebViewEvent::NonStringMessage`] with its JSON otherwise, and `omni-android`'s bridge script
//! calls exactly that. WebKit's channel is `window.webkit.messageHandlers.<name>.postMessage`, so a
//! first user script (`BRIDGE`) defines `window.chrome.webview.postMessage` over it: a string is
//! posted as `"s" + x`, anything else as `"j" + JSON.stringify(x)` (WebView2's
//! `get_WebMessageAsJson` is `JSON.stringify` too), so that the host always receives one string and
//! the two cases cannot be confused. Both user scripts run at document start in **every** frame, as
//! WebView2's `AddScriptToExecuteOnDocumentCreated` does, and -- also as on WebView2 -- only the
//! **main frame's** posts are delivered: `WKScriptMessage.frameInfo.isMainFrame`.
//!
//! # Events
//!
//! * `Ready` is sent once the window, the view and the scripts exist, immediately before the first
//!   `loadRequest:`; commands cannot arrive before it, because `open` returns after it.
//! * `NavigationStarting`: `webView:didStartProvisionalNavigation:` and each
//!   `webView:didReceiveServerRedirectForProvisionalNavigation:`, with `webView.URL`.
//! * `NavigationCompleted`: `webView:didFinishNavigation:` (success), and
//!   `webView:didFailProvisionalNavigation:withError:` / `webView:didFailNavigation:withError:`
//!   (failure), with the URL the last `NavigationStarting` named.
//! * `Failed`: the web content process terminated (`webViewWebContentProcessDidTerminate:`), a URL
//!   `NSURL` refused, or `evaluateJavaScript:` refused by WebKit for a reason other than the script
//!   throwing or returning a value WebKit cannot convert (the seam does not report a script's own
//!   result, and WebView2 reports neither as a failure).
//! * `Closed`: the person closed the window (`windowWillClose:` not caused by [`WebView::close`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSScreen, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    NSBundle, NSError, NSNotification, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURLRequest,
    NSURL,
};
use objc2_web_kit::{
    WKNavigation, WKNavigationDelegate, WKScriptMessage, WKScriptMessageHandler, WKUserContentController,
    WKUserScript, WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration,
};

use super::{Command, WebViewError, WebViewEvent, WebViewOptions, WebViewResult};
use crate::window::appkit_thread::{on_main, start_application, status, Status};

/// The script-message handler's name: `window.webkit.messageHandlers.omnidroid`.
const HANDLER: &str = "omnidroid";

/// The first user script: `window.chrome.webview.postMessage` over WebKit's message handler. See
/// this module's header, "The page's `postMessage`".
const BRIDGE: &str = r#"(() => {
  const handler = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.omnidroid;
  if (!handler) return;
  if (window.chrome && window.chrome.webview) return;
  const postMessage = (message) => {
    if (typeof message === "string") {
      handler.postMessage("s" + message);
      return;
    }
    let json;
    try { json = JSON.stringify(message); } catch (error) { json = undefined; }
    handler.postMessage("j" + (json === undefined ? "null" : json));
  };
  const webview = Object.freeze({ postMessage });
  if (!window.chrome) {
    Object.defineProperty(window, "chrome", { value: {}, enumerable: false, configurable: true, writable: true });
  }
  Object.defineProperty(window.chrome, "webview", { value: webview, enumerable: false, configurable: false, writable: false });
})();"#;

/// `WKErrorJavaScriptExceptionOccurred` and `WKErrorJavaScriptResultTypeIsUnsupported`
/// (`WKError.h`): the script ran; it threw, or returned something WebKit will not convert. Neither
/// is the host refusing the command.
const WK_ERROR_JAVASCRIPT_EXCEPTION: isize = 4;
const WK_ERROR_JAVASCRIPT_RESULT_UNSUPPORTED: isize = 5;

/// The installed WebKit's version: `com.apple.WebKit`'s `CFBundleVersion`, all numbers
/// (`21624.2.5.11.8` on the development host). Never opens a window.
pub(super) fn runtime_version() -> WebViewResult<String> {
    let id = NSString::from_str("com.apple.WebKit");
    let Some(bundle) = NSBundle::bundleWithIdentifier(&id) else {
        return Err(WebViewError::RuntimeMissing {
            detail: "NSBundle has no bundle with the identifier com.apple.WebKit: WebKit.framework \
                     is not loaded into this process"
                .to_owned(),
        });
    };
    let read = |key: &str| {
        bundle
            .objectForInfoDictionaryKey(&NSString::from_str(key))
            .and_then(|value| value.downcast::<NSString>().ok())
            .map(|value| value.to_string())
    };
    read("CFBundleVersion").ok_or_else(|| WebViewError::RuntimeMissing {
        detail: "com.apple.WebKit's Info.plist has no CFBundleVersion".to_owned(),
    })
}

// ------------------------------------------------------------------------ the delegate

/// What the delegate needs: where events go, the URL the current navigation named, and whether the
/// window is being closed by [`WebView::close`] rather than by the person.
struct DelegateIvars {
    events: Sender<WebViewEvent>,
    current_url: RefCell<String>,
    closing_by_request: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements; `WebViewDelegate` does not implement
    // `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelegateIvars]
    struct WebViewDelegate;

    unsafe impl NSObjectProtocol for WebViewDelegate {}

    unsafe impl WKNavigationDelegate for WebViewDelegate {
        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn did_start(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.starting(web_view);
        }

        #[unsafe(method(webView:didReceiveServerRedirectForProvisionalNavigation:))]
        fn did_redirect(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.starting(web_view);
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.completed(web_view, true);
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>, _error: &NSError) {
            self.completed(web_view, false);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>, _error: &NSError) {
            self.completed(web_view, false);
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn content_process_terminated(&self, _web_view: &WKWebView) {
            self.send(WebViewEvent::Failed(
                "webViewWebContentProcessDidTerminate: the page's web content process ended"
                    .to_owned(),
            ));
        }
    }

    unsafe impl WKScriptMessageHandler for WebViewDelegate {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            // SAFETY: both getters are plain property reads on a live message.
            let (main_frame, body) = unsafe { (message.frameInfo().isMainFrame(), message.body()) };
            if !main_frame {
                // WebView2 does not deliver a frame's posts to the view either (mod.rs, "The init
                // script, and what reaches iframes").
                return;
            }
            let event = match body.downcast::<NSString>() {
                Ok(text) => {
                    let text = text.to_string();
                    match text.split_at_checked(1) {
                        Some(("s", message)) => WebViewEvent::Message(message.to_owned()),
                        Some(("j", json)) => WebViewEvent::NonStringMessage { json: json.to_owned() },
                        _ => WebViewEvent::Failed(format!(
                            "a script message on `{HANDLER}` that the bridge did not frame: \
                             {} chars",
                            text.chars().count()
                        )),
                    }
                }
                Err(_) => WebViewEvent::Failed(format!(
                    "a script message on `{HANDLER}` whose body is not a string, which the bridge \
                     never posts"
                )),
            };
            self.send(event);
        }
    }

    unsafe impl NSWindowDelegate for WebViewDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            let ivars = self.ivars();
            ivars.closed.store(true, Ordering::Release);
            if !ivars.closing_by_request.load(Ordering::Acquire) {
                self.send(WebViewEvent::Closed);
            }
        }
    }
);

impl WebViewDelegate {
    fn new(mtm: MainThreadMarker, ivars: DelegateIvars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: `NSObject`'s designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn send(&self, event: WebViewEvent) {
        // A closed receiver means the handle is gone; the event has no one to reach.
        let _ = self.ivars().events.send(event);
    }

    fn url_of(web_view: &WKWebView) -> String {
        // SAFETY: a property read.
        unsafe { web_view.URL() }
            .and_then(|url| url.absoluteString())
            .map(|url| url.to_string())
            .unwrap_or_default()
    }

    fn starting(&self, web_view: &WKWebView) {
        let url = Self::url_of(web_view);
        self.ivars().current_url.replace(url.clone());
        self.send(WebViewEvent::NavigationStarting { url });
    }

    fn completed(&self, web_view: &WKWebView, success: bool) {
        let named = self.ivars().current_url.borrow().clone();
        let url = if named.is_empty() { Self::url_of(web_view) } else { named };
        self.send(WebViewEvent::NavigationCompleted { url, success });
    }
}

// ------------------------------------------------------------------------ the registry

/// Everything that must stay alive and is main-thread-only. `WKWebView.navigationDelegate` is weak,
/// so the delegate is held here too.
struct Native {
    window: Retained<NSWindow>,
    web_view: Retained<WKWebView>,
    delegate: Retained<WebViewDelegate>,
}

thread_local! {
    /// Main thread only: every access is inside `on_main`.
    static VIEWS: RefCell<HashMap<u64, Native>> = RefCell::new(HashMap::new());
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The caller's handle: an id in the main thread's registry, and the event channel.
pub(super) struct WebView {
    id: u64,
    events: Mutex<Receiver<WebViewEvent>>,
    /// Set by the window's `windowWillClose:`, whoever closed it.
    closed: Arc<AtomicBool>,
    /// Set by [`WebView::close`] before it closes the window, so that no `Closed` event is sent.
    closing_by_request: Arc<AtomicBool>,
}

impl WebView {
    pub(super) fn open(options: &WebViewOptions) -> WebViewResult<Self> {
        let state = status();
        if state != Status::Serving {
            return Err(WebViewError::MainThreadUnavailable { operation: "open", why: state.why() });
        }
        let (tx, rx) = channel();
        let closed = Arc::new(AtomicBool::new(false));
        let closing_by_request = Arc::new(AtomicBool::new(false));
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let options = options.clone();
        let ivars = DelegateIvars {
            events: tx,
            current_url: RefCell::new(String::new()),
            closing_by_request: Arc::clone(&closing_by_request),
            closed: Arc::clone(&closed),
        };
        on_main(move |mtm| build(mtm, id, &options, ivars))?;
        Ok(WebView { id, events: Mutex::new(rx), closed, closing_by_request })
    }

    pub(super) fn poll_events(&self) -> Vec<WebViewEvent> {
        let events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        events.try_iter().collect()
    }

    pub(super) fn send(&self, command: Command, operation: &'static str) -> WebViewResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(WebViewError::Closed { operation });
        }
        let id = self.id;
        match command {
            Command::Close => {
                self.close();
                Ok(())
            }
            Command::Navigate(url) => on_main(move |_| {
                VIEWS.with_borrow(|views| {
                    let Some(native) = views.get(&id) else { return Err(WebViewError::Closed { operation }) };
                    navigate(native, &url);
                    Ok(())
                })
            }),
            Command::ExecuteScript(script) => on_main(move |_| {
                VIEWS.with_borrow(|views| {
                    let Some(native) = views.get(&id) else { return Err(WebViewError::Closed { operation }) };
                    execute(native, &script);
                    Ok(())
                })
            }),
        }
    }

    pub(super) fn close(&self) {
        self.closing_by_request.store(true, Ordering::Release);
        let id = self.id;
        on_main(move |_| {
            let native = VIEWS.with_borrow_mut(|views| views.remove(&id));
            if let Some(native) = native {
                // SAFETY: plain calls on live objects on their own thread. The handler is removed so
                // the configuration's controller releases the delegate; the navigation delegate is
                // cleared before the view goes.
                unsafe {
                    native.web_view.stopLoading();
                    native.web_view.setNavigationDelegate(None);
                    native
                        .web_view
                        .configuration()
                        .userContentController()
                        .removeScriptMessageHandlerForName(&NSString::from_str(HANDLER));
                }
                native.window.setDelegate(None);
                if !self_closed(&native) {
                    native.window.close();
                }
                drop(native.delegate);
            }
        });
        // No flag to set here: the registry entry is gone, and every later command answers
        // `Closed` from its absence.
    }
}

/// Whether the window already went through `windowWillClose:` (the person closed it).
fn self_closed(native: &Native) -> bool {
    native.delegate.ivars().closed.load(Ordering::Acquire)
}

/// Build the window, the view and the scripts, send `Ready`, and issue the first navigation.
fn build(mtm: MainThreadMarker, id: u64, options: &WebViewOptions, ivars: DelegateIvars) -> WebViewResult<()> {
    start_application(mtm);
    let delegate = WebViewDelegate::new(mtm, ivars);

    let scale = NSScreen::mainScreen(mtm).map_or(1.0, |screen| screen.backingScaleFactor());
    let content = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(f64::from(options.width) / scale, f64::from(options.height) / scale),
    );
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    // SAFETY: the designated initializer, with a valid style and backing type.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            content,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the registry owns the window and closes it explicitly; it must not also release
    // itself on close.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(&options.title));

    // SAFETY: `WKWebViewConfiguration` and its controller are plain objects; the scripts are
    // strings validated by `mod.rs` (no NUL).
    let web_view = unsafe {
        let configuration = WKWebViewConfiguration::new(mtm);
        let controller = configuration.userContentController();
        controller.addScriptMessageHandler_name(
            ProtocolObject::from_ref(&*delegate),
            &NSString::from_str(HANDLER),
        );
        for source in std::iter::once(BRIDGE).chain(options.init_script.as_deref()) {
            let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &NSString::from_str(source),
                WKUserScriptInjectionTime::AtDocumentStart,
                false,
            );
            controller.addUserScript(&script);
        }
        let web_view = WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), content, &configuration);
        if let Some(agent) = &options.user_agent {
            web_view.setCustomUserAgent(Some(&NSString::from_str(agent)));
        }
        web_view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        web_view
    };
    window.setContentView(Some(&web_view));
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    window.center();
    window.makeKeyAndOrderFront(None);
    window.makeFirstResponder(Some(&web_view));
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);

    let native = Native { window, web_view, delegate };
    native.delegate.send(WebViewEvent::Ready);
    navigate(&native, &options.url);
    VIEWS.with_borrow_mut(|views| views.insert(id, native));
    Ok(())
}

/// `loadRequest:` for `url`; a URL `NSURL` will not parse is a `Failed` event, as WebView2 reports a
/// refused `Navigate`.
fn navigate(native: &Native, url: &str) {
    let Some(parsed) = NSURL::URLWithString(&NSString::from_str(url)) else {
        native.delegate.send(WebViewEvent::Failed(format!(
            "NSURL URLWithString: refused the URL ({} chars), so nothing was loaded",
            url.chars().count()
        )));
        return;
    };
    let request = NSURLRequest::requestWithURL(&parsed);
    // SAFETY: a live view on its own thread; the returned navigation is not needed.
    let _ = unsafe { native.web_view.loadRequest(&request) };
}

/// `evaluateJavaScript:completionHandler:`; see this module's header for which errors are events.
fn execute(native: &Native, script: &str) {
    let events = native.delegate.ivars().events.clone();
    let handler = block2::RcBlock::new(move |_result: *mut AnyObject, error: *mut NSError| {
        // SAFETY: WebKit passes a valid `NSError` or nil.
        let Some(error) = (unsafe { error.as_ref() }) else { return };
        let code = error.code();
        if code == WK_ERROR_JAVASCRIPT_EXCEPTION || code == WK_ERROR_JAVASCRIPT_RESULT_UNSUPPORTED {
            return;
        }
        let _ = events.send(WebViewEvent::Failed(format!(
            "evaluateJavaScript:completionHandler: failed: {} (domain {}, code {code})",
            error.localizedDescription(),
            error.domain()
        )));
    });
    // SAFETY: a live view on its own thread; the block is copied by WebKit.
    unsafe { native.web_view.evaluateJavaScript_completionHandler(&NSString::from_str(script), Some(&handler)) };
}
