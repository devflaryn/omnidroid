"""macOS rows: the web view seam's WKWebView backend (omni-platform webview/macos.rs). Pure data;
see `__init__.py`. The live tests open real windows, so the command sets their opt-in."""

WEBVIEW = "crates/omni-platform/src/webview/macos.rs"
LIVE = ["sh", "-c",
        "OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -p omni-platform --release --test webview_live "
        "--no-fail-fast -- --ignored --test-threads=1"]

ROWS = [
    ("mac-web-A1", "A", "a string post is delivered as JSON (the bridge's framing ignored)",
     WEBVIEW,
     """                        Some(("s", message)) => WebViewEvent::Message(message.to_owned()),""",
     """                        Some(("s", message)) => WebViewEvent::NonStringMessage { json: message.to_owned() },""",
     LIVE),
    ("mac-web-A2", "A", "the app's User-Agent is not applied",
     WEBVIEW,
     """            web_view.setCustomUserAgent(Some(&NSString::from_str(agent)));""",
     """            let _ = agent;""",
     LIVE),
    ("mac-web-A3", "A", "the init script is not installed (only the bridge is)",
     WEBVIEW,
     """        for source in std::iter::once(BRIDGE).chain(options.init_script.as_deref()) {""",
     """        for source in std::iter::once(BRIDGE) {""",
     LIVE),
    ("mac-web-A4", "A", "a refused navigation is reported as a success",
     WEBVIEW,
     """        fn did_fail_provisional(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>, _error: &NSError) {
            self.completed(web_view, false);""",
     """        fn did_fail_provisional(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>, _error: &NSError) {
            self.completed(web_view, true);""",
     LIVE),
    ("mac-web-A5", "A", "a command after close() is queued into nothing instead of answering Closed",
     WEBVIEW,
     """                    let Some(native) = views.get(&id) else { return Err(WebViewError::Closed { operation }) };
                    execute(native, &script);""",
     """                    let Some(native) = views.get(&id) else { return Ok(()) };
                    execute(native, &script);""",
     LIVE),
]
