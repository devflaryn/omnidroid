//! **A web page an app opens in "the browser", in a host browser window of its own** -- not in the
//! device's browser drawn over the app.
//!
//! An app shows web content in one of two ways. In a `WebView` of its own, inside its window: that
//! is the app's, drawn by the device's WebView in the app, and is left alone. Or by handing the URL
//! to the browser -- `startActivity` of an `ACTION_VIEW` intent for an `http(s)` URL, which is also
//! what a Custom Tab is -- and that is what this takes: the device's browser (`Browser2`) would take
//! the whole display, in the app's place, and leaving it was easy to get wrong. Instead the page
//! opens in a **host browser window** beside the device's (`omni_platform::webview`: WKWebView on
//! macOS, WebView2 on Windows), the app staying where it was, running and on screen. Closing that
//! window closes the page and nothing else.
//!
//! # How it is taken
//!
//! The host answers `IActivityTaskManager.startActivity` itself ([`crate::binder::Broker::intercept`])
//! when the intent is a browser's to take: action `VIEW`, an `http`/`https` URL, no component, and
//! no package or one other than the caller's (an app sending a link to itself is its own business).
//! The caller is told `START_SUCCESS`, as if the browser had started; the activity manager never
//! hears of it. Anything else goes on as before -- including on a host with no browser engine
//! (Linux), where the device's browser still takes the page.
//!
//! # A page that hands back to the app
//!
//! A sign-in or key page often ends by navigating to the app's own scheme (`roblox://...`, an
//! `intent:` URL). The host window does not load those: such a URL is handed to the device as an
//! `ACTION_VIEW` intent (`startActivity` as the shell), the app taking it as it would from a
//! browser, and the window closes.
//!
//! On by default where there is a browser engine; `OMNI_BROWSER_WINDOW=0` leaves pages to the
//! device's browser.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use omni_platform::webview::{WebView, WebViewEvent, WebViewOptions};
use parking_lot::Mutex;

use crate::binder::{Broker, Parcel};

/// `IActivityTaskManager` and its `startActivity` (`IActivityTaskManager$Stub.TRANSACTION_*`, read
/// from the image's `framework.jar`).
const ACTIVITY_TASK: &str = "android.app.IActivityTaskManager";
const START_ACTIVITY: u32 = 1;
const ACTION_VIEW: &str = "android.intent.action.VIEW";
/// `Intent.FLAG_ACTIVITY_NEW_TASK`, `UserHandle.USER_CURRENT`.
const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;
const USER_CURRENT: i32 = -2;
/// `flat_binder_object`'s `BINDER_TYPE_BINDER`: a null binder is one with no pointer, outside the
/// offsets (libbinder's `flattenBinder(nullptr)`).
const TYPE_BINDER: u32 = 0x7362_2a85;
/// The shell, as the host starts an activity (it may start one from the background).
const SHELL_UID: u32 = 2000;
const SHELL: &str = "com.android.shell";
/// A host browser window's size, in physical pixels (half that in points on a Retina screen).
const WIDTH: u32 = 1800;
const HEIGHT: u32 = 1300;

/// Whether pages go to a host browser window: unless `OMNI_BROWSER_WINDOW=0`.
#[must_use]
pub fn enabled() -> bool {
    std::env::var("OMNI_BROWSER_WINDOW").as_deref() != Ok("0")
}

/// Take the device's browser intents into host windows, once the activity manager is up (once per
/// host process). Nothing on a host without a browser engine.
pub fn start(broker: Arc<Broker>) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Err(e) = WebView::runtime_version() {
        eprintln!("[browser] no browser engine on this host ({e}): pages open in the device's browser");
        return;
    }
    let _ = std::thread::Builder::new().name("omni-browser".into()).spawn(move || run(&broker));
}

fn run(broker: &Arc<Broker>) {
    let handle = loop {
        match broker.check_service("activity_task") {
            Ok(Some(h)) => break h,
            Ok(None) => {}
            Err(e) => eprintln!("[browser] looking up the activity manager: {e}"),
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let (tx, rx) = mpsc::channel::<String>();
    let tx = Mutex::new(tx);
    let taken = broker.intercept(
        handle,
        START_ACTIVITY,
        Arc::new(move |parcel: &[u8]| {
            let url = browser_url(parcel)?;
            let _ = tx.lock().send(url);
            // writeNoException, ActivityManager.START_SUCCESS.
            Some([0i32.to_le_bytes(), 0i32.to_le_bytes()].concat())
        }),
    );
    if !taken {
        eprintln!("[browser] the activity manager's handle is gone: pages open in the device's browser");
        return;
    }
    eprintln!("[browser] web pages an app opens in the browser open in a host window (OMNI_BROWSER_WINDOW=0: the device's browser)");
    let mut open: Vec<WebView> = Vec::new();
    loop {
        match rx.recv_timeout(if open.is_empty() { Duration::from_secs(3600) } else { Duration::from_millis(100) }) {
            Ok(url) => {
                let title = format!("omnidroid — {}", host_of(&url));
                let options = WebViewOptions { title, url: url.clone(), width: WIDTH, height: HEIGHT, init_script: None, user_agent: None };
                match WebView::open(&options) {
                    Ok(view) => {
                        eprintln!("[browser] a page opened in a host window: {}", host_of(&url));
                        open.push(view);
                    }
                    Err(e) => eprintln!("[browser] the host window did not open: {e}"),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        open.retain(|view| {
            for event in view.poll_events() {
                match event {
                    WebViewEvent::Closed => {
                        eprintln!("[browser] the page's window closed");
                        return false;
                    }
                    WebViewEvent::NavigationStarting { url } if !is_web(&url) => {
                        match app_url(&url) {
                            Some(target) => match start_view(broker, handle, &target) {
                                Ok(code) => eprintln!("[browser] the page handed {} to the device (startActivity {code})", scheme_of(&target)),
                                Err(e) => eprintln!("[browser] the page's {} link: {e}", scheme_of(&target)),
                            },
                            None => eprintln!("[browser] the page opened a {} link with no app URL in it: not handed on", scheme_of(&url)),
                        }
                        view.close();
                        return false;
                    }
                    _ => {}
                }
            }
            true
        });
    }
}

/// The URL of a `startActivity` parcel the host's browser takes, or `None` to let it go on.
#[must_use]
pub fn browser_url(parcel: &[u8]) -> Option<String> {
    let mut r = Reader { b: parcel, at: 0 };
    // The interface token: strict-mode policy, work source, header, the interface's name.
    r.skip(12)?;
    if r.string16()?.as_deref() != Some(ACTIVITY_TASK) {
        return None;
    }
    // The caller's IApplicationThread (a flat_binder_object and its stability), its package and
    // feature, then the Intent (`writeTypedObject`).
    r.skip(28)?;
    let caller = r.string16()?;
    r.string16()?;
    if r.i32()? != 1 {
        return None;
    }
    // Intent.writeToParcel: action, data (Uri: its kind and its string), type, identifier, flags,
    // extended flags, package, component.
    let action = r.string8()?;
    let url = match r.i32()? {
        0 => None,
        1..=3 => r.string8()?,
        _ => return None,
    };
    r.string8()?;
    r.string8()?;
    r.i32()?;
    r.i32()?;
    let package = r.string8()?;
    let component = r.string16()?;
    let url = url?;
    let wanted = action.as_deref() == Some(ACTION_VIEW)
        && matches!(scheme_of(&url).as_str(), "http" | "https")
        && component.is_none()
        && (package.is_none() || package != caller);
    wanted.then_some(url)
}

/// Start `url` in the device as a browser would hand it on: `ACTION_VIEW`, a new task, as the shell.
/// The activity manager's answer (`START_SUCCESS` is 0).
fn start_view(broker: &Broker, handle: u32, url: &str) -> Result<i32, String> {
    let mut p = Parcel::with_interface_token(ACTIVITY_TASK);
    null_binder(&mut p);
    p.string16(SHELL);
    p.null_string16();
    p.i32(1);
    write_view_intent(&mut p, url);
    p.null_string16(); // resolvedType
    null_binder(&mut p); // resultTo
    p.null_string16(); // resultWho
    p.i32(-1); // requestCode
    p.i32(0); // flags
    p.i32(0); // profilerInfo
    p.i32(0); // options
    let (reply, _) = broker.host_transact_as(SHELL_UID, handle, START_ACTIVITY, p.bytes, &[]).map_err(|e| format!("startActivity: errno {}", e.0))?;
    let mut r = Reader { b: &reply, at: 0 };
    match r.i32() {
        Some(0) => r.i32().ok_or_else(|| "startActivity: a short reply".into()),
        Some(code) => Err(format!("startActivity: exception {code}")),
        None => Err("startActivity: an empty reply".into()),
    }
}

/// An `ACTION_VIEW` intent of `url` and `FLAG_ACTIVITY_NEW_TASK`, as Android 15's
/// `Intent.writeToParcel` writes one.
fn write_view_intent(p: &mut Parcel, url: &str) {
    p.string8(Some(ACTION_VIEW));
    p.i32(1); // a StringUri
    p.string8(Some(url));
    p.string8(None); // type
    p.string8(None); // identifier
    p.i32(FLAG_ACTIVITY_NEW_TASK);
    p.i32(0); // extended flags
    p.string8(None); // package
    p.null_string16(); // component
    p.i32(0); // source bounds
    p.i32(0); // categories
    p.i32(0); // selector
    p.i32(0); // clip data
    p.i32(USER_CURRENT); // content user hint
    p.i32(-1); // extras
    p.i32(0); // original intent
}

/// `writeStrongBinder(null)`: a binder object with no pointer (not in the offsets) and its
/// stability.
fn null_binder(p: &mut Parcel) {
    p.i32(TYPE_BINDER as i32);
    p.i32(0);
    p.i64(0);
    p.i64(0);
    p.i32(0);
}

/// The URL an app link names: itself for a scheme of its own, and for an `intent:` URL
/// (`intent://HOST/PATH#Intent;scheme=S;...;end`) the `S://HOST/PATH` it stands for.
#[must_use]
pub fn app_url(url: &str) -> Option<String> {
    let scheme = scheme_of(url);
    if scheme.is_empty() || scheme == "javascript" {
        return None;
    }
    if scheme != "intent" {
        return Some(url.to_string());
    }
    let (body, fragment) = url["intent:".len()..].split_once("#Intent;")?;
    let target = fragment.split(';').find_map(|kv| kv.strip_prefix("scheme="))?;
    if target.is_empty() || !target.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
        return None;
    }
    let rest = body.strip_prefix("//").unwrap_or(body);
    Some(format!("{target}://{rest}"))
}

/// Whether a host browser loads `url` itself.
fn is_web(url: &str) -> bool {
    matches!(scheme_of(url).as_str(), "http" | "https" | "about" | "data" | "blob" | "file")
}

fn scheme_of(url: &str) -> String {
    url.split_once(':').map_or("", |(s, _)| s).to_ascii_lowercase()
}

/// The host part of a URL, for the window's title and the log (never the path or query, which may
/// hold a key or a token).
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or("").rsplit('@').next().unwrap_or("").to_string()
}

/// A parcel read as `android.os.Parcel` reads one; every read is bounds-checked.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len())?;
        let out = &self.b[self.at..end];
        self.at = end;
        Some(out)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    /// `readString8`: `Some(None)` for a null string.
    fn string8(&mut self) -> Option<Option<String>> {
        let len = self.i32()?;
        if len < 0 {
            return Some(None);
        }
        let len = len as usize;
        let bytes = self.take(len.checked_add(4)? & !3)?;
        Some(Some(String::from_utf8_lossy(&bytes[..len]).into_owned()))
    }

    /// `readString16`: `Some(None)` for a null string.
    fn string16(&mut self) -> Option<Option<String>> {
        let len = self.i32()?;
        if len < 0 {
            return Some(None);
        }
        let len = len as usize;
        let bytes = self.take((len.checked_add(1)?.checked_mul(2)? + 3) & !3)?;
        let units: Vec<u16> = bytes[..len * 2].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(Some(String::from_utf16_lossy(&units)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `startActivity` parcel as the app's `IActivityTaskManager$Stub$Proxy` writes one, up to
    /// the intent's component (the rest is not read).
    fn start_activity(caller: &str, action: &str, uri: Option<(i32, &str)>, package: Option<&str>, component: Option<(&str, &str)>) -> Vec<u8> {
        let mut p = Parcel::with_interface_token(ACTIVITY_TASK);
        null_binder(&mut p);
        p.string16(caller);
        p.null_string16();
        p.i32(1);
        p.string8(Some(action));
        match uri {
            Some((kind, s)) => {
                p.i32(kind);
                p.string8(Some(s));
            }
            None => p.i32(0),
        }
        p.string8(None);
        p.string8(None);
        p.i32(0x1000_0000);
        p.i32(0);
        p.string8(package);
        match component {
            Some((pkg, cls)) => {
                p.string16(pkg);
                p.string16(cls);
            }
            None => p.null_string16(),
        }
        p.i32(0);
        p.bytes
    }

    #[test]
    fn a_web_page_for_the_browser_is_taken() {
        let url = "https://example.com/key?token=1";
        assert_eq!(browser_url(&start_activity("com.roblox.client", ACTION_VIEW, Some((1, url)), None, None)).as_deref(), Some(url));
        // A hierarchical Uri (Uri.Builder), and a Custom Tab naming a browser's package.
        assert_eq!(browser_url(&start_activity("com.roblox.client", ACTION_VIEW, Some((3, url)), Some("com.android.chrome"), None)).as_deref(), Some(url));
        assert!(browser_url(&start_activity("x", ACTION_VIEW, Some((1, "HTTP://EXAMPLE.COM")), None, None)).is_some());
    }

    #[test]
    fn everything_else_goes_on() {
        let url = "https://example.com/";
        let explicit = start_activity("com.roblox.client", ACTION_VIEW, Some((1, url)), None, Some(("org.chromium.webview_shell", "Main")));
        assert_eq!(browser_url(&explicit), None, "an explicit component");
        assert_eq!(browser_url(&start_activity("com.roblox.client", ACTION_VIEW, Some((1, url)), Some("com.roblox.client"), None)), None, "a link to itself");
        assert_eq!(browser_url(&start_activity("x", "android.intent.action.MAIN", Some((1, url)), None, None)), None);
        assert_eq!(browser_url(&start_activity("x", ACTION_VIEW, Some((1, "roblox://placeId=1")), None, None)), None);
        assert_eq!(browser_url(&start_activity("x", ACTION_VIEW, None, None, None)), None);
        let mut short = start_activity("x", ACTION_VIEW, Some((1, url)), None, None);
        short.truncate(short.len() - 12);
        assert_eq!(browser_url(&short), None, "a parcel that ends early");
        let mut other = Parcel::with_interface_token("android.app.IActivityManager");
        other.i32(0);
        assert_eq!(browser_url(&other.bytes), None);
    }

    #[test]
    fn the_view_intent_reads_back() {
        let mut p = Parcel::with_interface_token(ACTIVITY_TASK);
        null_binder(&mut p);
        p.string16(SHELL);
        p.null_string16();
        p.i32(1);
        write_view_intent(&mut p, "https://example.com/a");
        assert_eq!(browser_url(&p.bytes).as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn app_links_are_handed_back() {
        assert_eq!(app_url("roblox://placeId=1").as_deref(), Some("roblox://placeId=1"));
        assert_eq!(app_url("intent://auth/cb?x=1#Intent;scheme=myapp;package=a.b;end").as_deref(), Some("myapp://auth/cb?x=1"));
        assert_eq!(app_url("intent://x#Intent;package=a.b;end"), None, "no scheme");
        assert_eq!(app_url("intent://x#Intent;scheme=a b;end"), None);
        assert_eq!(app_url("javascript:alert(1)"), None);
    }

    #[test]
    fn the_title_names_the_host_only() {
        assert_eq!(host_of("https://user:pw@keys.example.com:8443/path?k=secret"), "keys.example.com:8443");
        assert_eq!(host_of("https://example.com"), "example.com");
    }
}
