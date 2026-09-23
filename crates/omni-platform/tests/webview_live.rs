//! Tests that open a **real** WebView2 window, and therefore need a desktop session and the
//! WebView2 runtime.
//!
//! # The gate
//!
//! The arrangement of `tests/audio_live.rs` and `tests/window_live.rs` (VERIFICATION entry 4: a
//! test that cannot run must fail, not skip): every test is `#[ignore]`d with a reason naming the
//! variable, and under `--ignored` without `OMNI_WEBVIEW_LIVE_TESTS=1` it **panics** naming it.
//!
//! ```text
//! OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -p omni-platform --release --test webview_live -- --ignored --nocapture
//! ```
//!
//! # No network
//!
//! Every page is a `data:` URL or is served by a listener this file opens on `127.0.0.1`, so the
//! results depend on nothing outside the machine.
//!
//! # What each assertion could catch
//!
//! Every positive result has a control that a broken implementation would fail differently: the
//! navigation that must succeed has a twin that must **fail** (a refused port), so `success` is not
//! a constant; the init-script value is one the page cannot produce itself; the User-Agent is one
//! no browser sends by default, and it is checked both where the page reads it and in the HTTP
//! request that fetched the page; and no healthy test may see a single `Failed` event (VERIFICATION
//! entry 16: a failure nobody asserts on is a green run on a burning building).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use omni_platform::webview::{WebView, WebViewError, WebViewEvent, WebViewOptions};

/// The opt-in.
const GATE: &str = "OMNI_WEBVIEW_LIVE_TESTS";

/// Fail — loudly, naming the variable — if these were run without the opt-in.
fn require_gate() {
    let set = std::env::var(GATE).is_ok_and(|v| v == "1");
    assert!(
        set,
        "this test was run with --ignored but {GATE} is not set to 1. It opens a real WebView2 \
         window and needs a desktop session and the WebView2 runtime; it will not pretend to have \
         passed without one. Set {GATE}=1 to run it, or drop --ignored to skip it visibly."
    );
}

/// How long anything may take. Generous: a cold WebView2 start launches a browser process.
const PATIENCE: Duration = Duration::from_secs(30);

/// `html` as a `data:text/html` URL, with every byte but the unreserved ones percent-encoded, so
/// that nothing in the page can be read as URL syntax.
fn data_url(html: &str) -> String {
    let mut url = String::from("data:text/html;charset=utf-8,");
    for byte in html.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

fn options(title: &str, url: String) -> WebViewOptions {
    WebViewOptions {
        title: format!("omnidroid webview_live: {title}"),
        url,
        width: 640,
        height: 480,
        init_script: None,
        user_agent: None,
    }
}

fn open(options: &WebViewOptions) -> WebView {
    WebView::open(options).unwrap_or_else(|error| panic!("WebView::open failed: {error}"))
}

/// Everything a test has seen, in order.
struct Seen {
    view: WebView,
    events: Vec<WebViewEvent>,
    /// Events before this index have been matched by an `until` (or passed over by one), so the
    /// next `until` looks only after it: waits are ordered, and an event that arrived in the same
    /// poll as the previous one is still found.
    cursor: usize,
}

impl Seen {
    fn new(view: WebView) -> Self {
        Seen { view, events: Vec::new(), cursor: 0 }
    }

    /// Poll until an event at or after the cursor satisfies `done`, move the cursor past it and
    /// return it; panic with every event seen if `PATIENCE` passes first.
    fn until(&mut self, what: &str, done: impl Fn(&WebViewEvent) -> bool) -> WebViewEvent {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.events.extend(self.view.poll_events());
            if let Some(at) = self.events[self.cursor..].iter().position(&done) {
                let at = self.cursor + at;
                self.cursor = at + 1;
                return self.events[at].clone();
            }
            assert!(Instant::now() < deadline, "no {what} within {PATIENCE:?}; saw {:#?}", self.events);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Keep polling for `period`, collecting whatever arrives.
    fn soak(&mut self, period: Duration) {
        let end = Instant::now() + period;
        while Instant::now() < end {
            self.events.extend(self.view.poll_events());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn completed(&mut self) -> (String, bool) {
        match self.until("NavigationCompleted", |e| matches!(e, WebViewEvent::NavigationCompleted { .. })) {
            WebViewEvent::NavigationCompleted { url, success } => (url, success),
            _ => unreachable!("matched above"),
        }
    }

    fn message(&mut self, text: &str) {
        self.until(&format!("Message({text:?})"), |e| *e == WebViewEvent::Message(text.to_owned()));
    }

    fn messages(&self) -> Vec<&str> {
        self.events
            .iter()
            .filter_map(|e| match e { WebViewEvent::Message(text) => Some(text.as_str()), _ => None })
            .collect()
    }

    fn assert_no_failure(&self) {
        let failed: Vec<_> = self.events.iter().filter(|e| matches!(e, WebViewEvent::Failed(_))).collect();
        assert!(failed.is_empty(), "a healthy run reported {failed:#?}; all events {:#?}", self.events);
    }
}

#[test]
#[ignore = "needs the WebView2 runtime: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
fn the_runtime_version_answers_without_a_window() {
    require_gate();
    let version = WebView::runtime_version().expect("an installed WebView2 runtime");
    println!("WebView2 runtime {version}");
    let parts: Vec<u32> = version.split('.').map(|p| p.parse().expect("numeric")).collect();
    assert_eq!(parts.len(), 4, "{version}");
    assert!(parts[0] >= 86, "{version} is older than the loader's minimum");
}

/// The first event is `Ready`, then the navigation starts and completes, and the page's
/// `postMessage('hello')` arrives as that string.
#[test]
#[ignore = "opens a real window: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
fn a_page_that_posts_hello_arrives_as_a_message() {
    require_gate();
    let url = data_url("<p>hello</p><script>chrome.webview.postMessage('hello')</script>");
    let started = Instant::now();
    let mut seen = Seen::new(open(&options("hello", url.clone())));
    seen.message("hello");
    let (done, success) = seen.completed();
    println!("hello after {:?}; events {:#?}", started.elapsed(), seen.events);

    assert_eq!(seen.events.first(), Some(&WebViewEvent::Ready), "{:#?}", seen.events);
    assert!(success, "{:#?}", seen.events);
    assert_eq!(done, url, "the completed URL is the one navigated to");
    assert!(seen.events.contains(&WebViewEvent::NavigationStarting { url: url.clone() }), "{:#?}", seen.events);
    seen.assert_no_failure();
}

/// The init script defines what the Android side's JavaScript interface will be — an object whose
/// methods forward through `postMessage` — and the page's **inline, parse-time** script calls it.
/// That only works if the init script ran first. It runs again in the next document.
#[test]
#[ignore = "opens a real window: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
fn the_init_script_runs_before_the_page_in_every_document() {
    require_gate();
    let init = "window.OmniBridge = { send: function (s) { window.chrome.webview.postMessage('bridge:' + s); } };";
    let page = |name: &str| {
        data_url(&format!(
            "<script>OmniBridge.send('{name}:' + (typeof OmniBridge) + ':' + document.readyState)</script>"
        ))
    };
    let mut seen = Seen::new(open(&WebViewOptions {
        init_script: Some(init.to_owned()),
        ..options("init script", page("first"))
    }));
    seen.message("bridge:first:object:loading");
    seen.completed();
    seen.view.navigate(&page("second")).unwrap();
    seen.message("bridge:second:object:loading");
    seen.assert_no_failure();
}

/// `postMessage({a: 1})` is not a string: it is reported as one that is not, with its JSON.
#[test]
#[ignore = "opens a real window: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
fn a_non_string_post_is_named_not_dropped() {
    require_gate();
    let url = data_url("<script>chrome.webview.postMessage({a: 1}); chrome.webview.postMessage('after')</script>");
    let mut seen = Seen::new(open(&options("non-string", url)));
    seen.message("after");
    let json: Vec<_> = seen
        .events
        .iter()
        .filter_map(|e| match e { WebViewEvent::NonStringMessage { json } => Some(json.as_str()), _ => None })
        .collect();
    assert_eq!(json, ["{\"a\":1}"], "{:#?}", seen.events);
    seen.assert_no_failure();
}

/// `execute_script` runs in the loaded page, and its `postMessage` arrives.
#[test]
#[ignore = "opens a real window: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
fn execute_script_runs_in_the_page() {
    require_gate();
    let mut seen = Seen::new(open(&options("execute_script", data_url("<title>exec</title>"))));
    seen.completed();
    seen.view.execute_script("chrome.webview.postMessage('exec:' + document.title)").unwrap();
    seen.message("exec:exec");
    seen.assert_no_failure();
}

/// `navigate` to a second page completes with `success: true` — and a navigation to a port
/// nothing listens on completes with `success: false`, so the flag is read, not assumed.
#[test]
#[ignore = "opens a real window: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
fn navigate_completes_and_a_refused_one_is_not_a_success() {
    require_gate();
    let mut seen = Seen::new(open(&options("navigate", data_url("<p>one</p>"))));
    assert!(seen.completed().1, "{:#?}", seen.events);

    let second = data_url("<p>two</p>");
    seen.view.navigate(&second).unwrap();
    assert_eq!(seen.completed(), (second, true));

    // Bound and dropped: a port that was just free and now refuses.
    let refused = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port())
    };
    seen.view.navigate(&refused).unwrap();
    let (url, success) = seen.completed();
    println!("refused navigation completed as ({url}, {success}); events {:#?}", seen.events);
    assert_eq!((url.as_str(), success), (refused.as_str(), false));
    seen.assert_no_failure();
}

/// Serve `page` to every request on `listener` for `PATIENCE`, and hand back each request's
/// `User-Agent` header.
fn serve(listener: TcpListener, page: String) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    listener.set_nonblocking(false).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buffer[..n]),
                }
            }
            let request = String::from_utf8_lossy(&request).into_owned();
            let agent = request
                .lines()
                .find_map(|line| line.strip_prefix("User-Agent: ").or_else(|| line.strip_prefix("user-agent: ")))
                .unwrap_or("<no User-Agent header>")
                .to_owned();
            let _ = tx.send(agent);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    rx
}

/// The User-Agent the Roblox app sets is what the page reads **and** what the request carried.
#[test]
#[ignore = "opens a real window: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
fn the_user_agent_is_what_the_page_and_the_server_see() {
    require_gate();
    let agent = "Mozilla/5.0 (Linux; Android 14; omnidroid) ROBLOX Android App 2.738.1397 Phone Hybrid()";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/challenge", listener.local_addr().unwrap().port());
    let requests = serve(listener, "<script>chrome.webview.postMessage(navigator.userAgent)</script>".into());

    let mut seen = Seen::new(open(&WebViewOptions { user_agent: Some(agent.to_owned()), ..options("user agent", url) }));
    seen.message(agent);
    let first = requests.recv_timeout(PATIENCE).expect("the page was requested");
    assert_eq!(first, agent, "the HTTP request's User-Agent");
    seen.assert_no_failure();
}

/// `close` ends it: every command after answers `Closed`, a second `close` is a no-op, and the
/// caller's own close is not reported back to it as the person closing the window.
#[test]
#[ignore = "opens a real window: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
fn after_close_every_command_answers_closed() {
    require_gate();
    let mut seen = Seen::new(open(&options("close", data_url("<p>bye</p>"))));
    seen.completed();
    seen.view.close();
    assert_eq!(seen.view.execute_script("1"), Err(WebViewError::Closed { operation: "execute_script" }));
    assert_eq!(seen.view.navigate("data:text/html,x"), Err(WebViewError::Closed { operation: "navigate" }));
    seen.view.close();
    seen.events.extend(seen.view.poll_events());
    assert!(!seen.events.contains(&WebViewEvent::Closed), "{:#?}", seen.events);
    seen.assert_no_failure();
}

/// A `close` before the browser is even ready is still a clean end.
#[test]
#[ignore = "opens a real window: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
fn close_before_ready_ends_cleanly() {
    require_gate();
    let view = open(&options("early close", data_url("<p>never</p>")));
    view.close();
    assert_eq!(view.navigate("data:text/html,x"), Err(WebViewError::Closed { operation: "navigate" }));
}

/// **A measurement**: what the init script and `postMessage` do inside iframes, which is where a
/// captcha provider may put its UI. Two frames, both cross-document: a `srcdoc` frame (same origin
/// as the page) and a `data:` frame (an opaque, cross-origin one). Each reports, through the
/// ordinary `window.parent.postMessage` that the top page relays, whether the init script ran in it
/// and whether it has `window.chrome.webview`; then it tries `chrome.webview.postMessage` itself.
/// The assertions pin what was measured (see `src/webview/mod.rs`, "MEASURED").
///
/// The page also posts `document.hasFocus()` at `load`, **printed and not asserted**: whether this
/// process may bring a window to the foreground is the foreground lock's decision (it depends on
/// which process last had input), not this seam's.
#[test]
#[ignore = "opens a real window: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
fn what_reaches_an_iframe() {
    require_gate();
    let init = "window.__omni_init = true; if (window.chrome && window.chrome.webview) { \
                window.chrome.webview.postMessage('init-ran-in-' + (window === window.top ? 'top' : 'a-frame')); }";
    let frame = |kind: &str| {
        format!(
            "<script>var w = !!(window.chrome && window.chrome.webview); \
             parent.postMessage('{kind}:init=' + (typeof window.__omni_init) + ',webview=' + w, '*'); \
             if (w) {{ window.chrome.webview.postMessage('{kind}:direct'); }}</script>"
        )
    };
    let page = format!(
        "<script>window.addEventListener('message', function (e) {{ chrome.webview.postMessage('relay:' + e.data); }}); \
         chrome.webview.postMessage('top:init=' + (typeof window.__omni_init)); \
         window.addEventListener('load', function () {{ chrome.webview.postMessage('focus=' + document.hasFocus()); }});</script>\
         <input autofocus><iframe srcdoc=\"{}\"></iframe><iframe src=\"{}\"></iframe>",
        frame("srcdoc").replace('"', "&quot;"),
        data_url(&frame("data"))
    );
    let mut seen =
        Seen::new(open(&WebViewOptions { init_script: Some(init.to_owned()), ..options("iframes", data_url(&page)) }));
    // In either order: the two frames load independently.
    let deadline = Instant::now() + PATIENCE;
    while !["relay:srcdoc:", "relay:data:"].iter().all(|p| seen.messages().iter().any(|m| m.starts_with(p))) {
        assert!(Instant::now() < deadline, "the frames did not both report: {:#?}", seen.events);
        seen.soak(Duration::from_millis(20));
    }
    // Anything a frame posts directly would arrive about as fast as its relayed report; wait well
    // past that before concluding it did not.
    seen.soak(Duration::from_secs(3));
    println!("iframe measurement, every message in order: {:#?}", seen.messages());
    let messages = seen.messages();

    // The init script ran in the top document, before its inline script.
    assert!(messages.contains(&"init-ran-in-top"), "{messages:#?}");
    assert!(messages.contains(&"top:init=boolean"), "{messages:#?}");
    // It ran in **both** frames too — same-origin and cross-origin — and both have
    // `window.chrome.webview`, as WebView2.idl says ("all top-level document and child frame
    // page navigations").
    assert!(messages.contains(&"relay:srcdoc:init=boolean,webview=true"), "{messages:#?}");
    assert!(messages.contains(&"relay:data:init=boolean,webview=true"), "{messages:#?}");
    // But **nothing a frame posts through `chrome.webview.postMessage` reaches this seam**: not
    // the init script's own post from inside a frame, not the frames' direct posts. WebView2.idl
    // routes those to `ICoreWebView2Frame2::add_WebMessageReceived`, which this seam does not
    // register. If a runtime ever changes that, this fails and the module docs must change.
    let from_frames: Vec<_> =
        messages.iter().filter(|m| m.starts_with("init-ran-in-a-frame") || m.ends_with(":direct")).collect();
    assert!(from_frames.is_empty(), "a frame's post reached the top-level handler: {from_frames:?}");
    seen.assert_no_failure();
}
