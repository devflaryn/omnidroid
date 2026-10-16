//! **The web pages in an instance, for coding agents**: every `WebView` an app shows -- a sign-in
//! form, a captcha, a store page -- read and driven as a browser's DevTools would, with no
//! browser of its own in the way: its DOM listed (each element with where it is on the instance's
//! display), JavaScript run in it (in any of its frames), an element clicked by selector, text or
//! the `ref` the listing gave, a point tapped, text typed, the page captured.
//!
//! **How a page is reached.** The device's WebView serves the Chrome DevTools Protocol on an
//! abstract socket of the app's process, `@webview_devtools_remote_<pid>` (on for every app: the
//! image is a `userdebug` build). Abstract sockets cross host processes through each one's broker
//! (`omni_linux::xsocket`); the server connects through the same broker
//! ([`omni_linux::xsocket::HostStream`]), as the shell user, and speaks HTTP (`/json`: the pages)
//! and a WebSocket (a page's protocol) over it.
//!
//! **Where an element is.** `DOM.getBoxModel` answers in CSS pixels of the page's viewport; the
//! page's `/json` entry says where its WebView is on the display and how wide, in device pixels,
//! and `Page.getLayoutMetrics` how wide the viewport is in CSS pixels and where it has scrolled
//! to -- together, the display point an Android tap lands on. A click is such a tap by default
//! (the touchscreen's own path: what a person does), or the protocol's mouse events (`via: cdp`).
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use omni_linux::xsocket::HostStream;
use omni_mcp::json::{self, Json};
use omni_mcp::mcp::RpcError;
use omni_pool::{InstanceState, Pool};

/// The DevTools sockets' prefix.
const DEVTOOLS: &str = "@webview_devtools_remote_";

/// Who the server connects as: the shell user, whom the WebView lets in (`pid`, `uid`, `gid`).
const SHELL: [u32; 3] = [0, 2000, 2000];

/// How long one protocol call may take.
const CALL: Duration = Duration::from_secs(30);

/// The tools, as `live.rs` lists them.
pub const TOOLS: &[super::live::ToolDef] = &[
    ("web_pages", "The web pages (WebViews) an instance's app shows: page id, url, title, whether visible, and where its WebView is on the display. The other web_* tools take `page` (an id or the start of one) and default to the visible page.", &[("instance_id", "number", true, "the instance")]),
    ("web_dom", "The page's elements as a list -- every frame, captcha iframes too: `ref`, tag, the telling attributes (id, class, name, type, role, aria-label, placeholder, href, value...), text, the frame's url, the box in CSS pixels, and `tap` -- the display point an `input` tap or web_tap hits it at. Default: what can be clicked or typed into, headings and short text; `selector` (CSS) lists those matches instead, with `html` their outer HTML.", &[("instance_id", "number", true, "the instance"), ("page", "string", false, "page id (web_pages); default the visible one"), ("selector", "string", false, "a CSS selector, matched in every frame"), ("html", "boolean", false, "with selector: each match's outer HTML (cut at 4000 characters)"), ("limit", "number", false, "most elements answered (default 200)")]),
    ("web_eval", "Run JavaScript in the page and answer its value (awaited if a promise; JSON-able values as they are), as a user gesture. With `frame`, in every frame whose url contains it -- an iframe's, a captcha's, whatever its origin -- answering each frame's value or error.", &[("instance_id", "number", true, "the instance"), ("expression", "string", true, "the JavaScript"), ("page", "string", false, "page id; default the visible one"), ("frame", "string", false, "part of a frame's url (an iframe's: a captcha's)")]),
    ("web_click", "Click an element: by `ref` (web_dom), `selector` (CSS, every frame; `index` picks among matches) or `text` (what it shows). It is scrolled into view, then tapped at its centre through the display's touchscreen (`via: touch`, the default: as a finger does) or clicked with DevTools mouse events (`via: cdp`). Answers what was clicked and where.", &[("instance_id", "number", true, "the instance"), ("ref", "number", false, "an element's ref from web_dom"), ("selector", "string", false, "a CSS selector"), ("text", "string", false, "text the element shows (case-insensitive, part of it)"), ("index", "number", false, "which match (default 0)"), ("via", "string", false, "touch (default) or cdp"), ("page", "string", false, "page id; default the visible one")]),
    ("web_tap", "Click a point of the page: `x`,`y` in the page's CSS pixels (as web_dom's boxes and getBoundingClientRect give them; `space: screen` for the display's pixels instead). Through the touchscreen by default, DevTools mouse events with `via: cdp`. Answers the display point.", &[("instance_id", "number", true, "the instance"), ("x", "number", true, "x"), ("y", "number", true, "y"), ("space", "string", false, "css (default) or screen"), ("via", "string", false, "touch (default) or cdp"), ("page", "string", false, "page id; default the visible one")]),
    ("web_type", "Type text into the page: into the element given (`ref`, `selector` or `text`, which is clicked first) or wherever the focus is. `submit: true` presses Enter after.", &[("instance_id", "number", true, "the instance"), ("text", "string", true, "the text"), ("ref", "number", false, "an element's ref from web_dom"), ("selector", "string", false, "a CSS selector"), ("submit", "boolean", false, "press Enter after"), ("page", "string", false, "page id; default the visible one")]),
    ("web_screenshot", "The page as the WebView renders it (every frame), as a PNG: its path, width and height.", &[("instance_id", "number", true, "the instance"), ("page", "string", false, "page id; default the visible one")]),
];

fn err(e: impl std::fmt::Display) -> RpcError {
    RpcError::server(e.to_string())
}

fn num(v: f64) -> Json {
    Json::Num((v * 10.0).round() / 10.0)
}

fn arg_str<'a>(args: &'a Json, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Json::as_str)
}

fn arg_f64(args: &Json, name: &str) -> Option<f64> {
    args.get(name).and_then(Json::as_f64).or_else(|| arg_str(args, name).and_then(|s| s.parse().ok()))
}

fn arg_bool(args: &Json, name: &str) -> bool {
    args.get(name).and_then(Json::as_bool).unwrap_or(false) || arg_str(args, name) == Some("true")
}

/// An object from pairs with keys made at run time.
fn object(pairs: Vec<(String, Json)>) -> Json {
    Json::Object(pairs.into_iter().collect())
}

// ---------------------------------------------------------------------------------------------
// The connection: HTTP and a WebSocket over the guest's socket.

/// An HTTP `GET` to the DevTools socket `name`: the body.
fn http_get(dir: &Path, name: &[u8], path: &str) -> Result<String, String> {
    let mut s = HostStream::connect(dir, name, SHELL).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(CALL)).map_err(|e| e.to_string())?;
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut got = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
            let length = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok());
            if let Some(n) = length {
                if got.len() >= end + 4 + n {
                    return Ok(String::from_utf8_lossy(&got[end + 4..end + 4 + n]).into_owned());
                }
            }
        }
        let n = s.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            let at = got.windows(4).position(|w| w == b"\r\n\r\n").map_or(0, |e| e + 4);
            return Ok(String::from_utf8_lossy(&got[at..]).into_owned());
        }
        got.extend_from_slice(&buf[..n]);
    }
}

/// One page's protocol, over a WebSocket.
struct Cdp {
    s: HostStream,
    next: u64,
    /// Events that came while a call waited for its answer.
    events: Vec<Json>,
}

impl Cdp {
    fn open(dir: &Path, name: &[u8], page: &str) -> Result<Self, String> {
        let mut s = HostStream::connect(dir, name, SHELL).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(CALL)).map_err(|e| e.to_string())?;
        let request = format!("GET /devtools/page/{page} HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: b21uaWRyb2lkLW1jcC0xMg==\r\nSec-WebSocket-Version: 13\r\n\r\n");
        s.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
        // The answer's head, a byte at a time: the frames that follow are the protocol's.
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if s.read(&mut b).map_err(|e| e.to_string())? == 0 || head.len() > 16 << 10 {
                return Err("the DevTools socket closed before the WebSocket opened".into());
            }
            head.push(b[0]);
        }
        let head = String::from_utf8_lossy(&head);
        if !head.starts_with("HTTP/1.1 101") {
            return Err(format!("the page's WebSocket: {}", head.lines().next().unwrap_or_default()));
        }
        Ok(Self { s, next: 1, events: Vec::new() })
    }

    /// A client frame (masked, as a client's must be).
    fn send(&mut self, op: u8, payload: &[u8]) -> std::io::Result<()> {
        let mut frame = vec![0x80 | op];
        match payload.len() {
            n if n < 126 => frame.push(0x80 | n as u8),
            n if n < 65536 => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let mask: [u8; 4] = (self.next as u32 ^ 0x5a17_c3e1).to_le_bytes();
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.s.write_all(&frame)
    }

    /// The next text message (control frames answered on the way).
    fn receive(&mut self) -> Result<String, String> {
        let mut message = Vec::new();
        loop {
            let mut h = [0u8; 2];
            self.s.read_exact(&mut h).map_err(|e| format!("the page's WebSocket: {e}"))?;
            let (fin, op) = (h[0] & 0x80 != 0, h[0] & 0x0f);
            let mut len = u64::from(h[1] & 0x7f);
            if len == 126 {
                let mut b = [0u8; 2];
                self.s.read_exact(&mut b).map_err(|e| e.to_string())?;
                len = u64::from(u16::from_be_bytes(b));
            } else if len == 127 {
                let mut b = [0u8; 8];
                self.s.read_exact(&mut b).map_err(|e| e.to_string())?;
                len = u64::from_be_bytes(b);
            }
            if len > 256 << 20 {
                return Err("a WebSocket frame over 256 MiB".into());
            }
            let mut payload = vec![0u8; len as usize];
            self.s.read_exact(&mut payload).map_err(|e| e.to_string())?;
            match op {
                0x8 => return Err("the page closed its WebSocket (navigated away or closed?)".into()),
                0x9 => self.send(0xA, &payload).map_err(|e| e.to_string())?,
                0xA => {}
                _ => {
                    message.extend_from_slice(&payload);
                    if fin {
                        return Ok(String::from_utf8_lossy(&message).into_owned());
                    }
                }
            }
        }
    }

    /// Call `method`: its result, or the protocol's error.
    fn call(&mut self, method: &str, params: Json) -> Result<Json, String> {
        let id = self.next;
        self.next += 1;
        let request = Json::Object([("id".to_string(), Json::Num(id as f64)), ("method".to_string(), json::s(method)), ("params".to_string(), params)].into_iter().collect());
        self.send(0x1, request.to_string().as_bytes()).map_err(|e| e.to_string())?;
        loop {
            let text = self.receive()?;
            let Ok(m) = json::parse(&text) else { continue };
            if m.get("id").and_then(Json::as_u64) == Some(id) {
                if let Some(e) = m.get("error") {
                    return Err(format!("{method}: {}", e.get("message").and_then(Json::as_str).unwrap_or("error")));
                }
                return Ok(m.get("result").cloned().unwrap_or(Json::Null));
            }
            if self.events.len() < 1000 {
                self.events.push(m);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The pages of an instance.

/// A page, as its WebView's `/json` lists it.
struct Page {
    socket: Vec<u8>,
    id: String,
    url: String,
    title: String,
    visible: bool,
    /// Its WebView's place on the display, device pixels: left, top, width, height.
    screen: [f64; 4],
}

/// The instance's Android's directory, its display, and its app's processes (pids).
struct Target {
    dir: PathBuf,
    device: u32,
    display: i32,
    pids: Vec<u32>,
}

/// The processes of `package` for `user`, from the activity manager:
/// `*APP* UID 1010087 ProcessRecord{8cdebf1 416000:org.example/u10a87}`.
fn app_pids(dump: &str, package: &str, user: u32) -> Vec<u32> {
    dump.lines()
        .filter_map(|l| {
            let rec = l.split("ProcessRecord{").nth(1)?;
            let (pid, rest) = rec.split_whitespace().nth(1)?.split_once(':')?;
            let (name, owner) = rest.trim_end_matches('}').rsplit_once('/')?;
            let u: u32 = owner.strip_prefix('u')?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
            ((name == package || name.starts_with(&format!("{package}:"))) && u == user).then(|| pid.parse().ok())?
        })
        .collect()
}

fn target(pool: &Pool, id: u32) -> Result<Target, RpcError> {
    let (devices, instances) = pool.snapshot();
    let i = instances.iter().find(|i| i.id == id).ok_or_else(|| RpcError::params(format!("no instance #{id}")))?;
    let (InstanceState::Running, Some(display)) = (&i.state, i.display) else {
        return Err(RpcError::server(format!("#{id} is not running")));
    };
    let d = devices.iter().find(|d| d.id == i.device).ok_or_else(|| RpcError::server(format!("its Android D{} is gone", i.device)))?;
    let dev = pool.device(i.device).ok_or_else(|| RpcError::server(format!("its Android D{} is gone", i.device)))?;
    let (_, dump) = dev.shell_as("dumpsys activity processes | grep ProcessRecord", Some(0), Duration::from_secs(30)).map_err(err)?;
    Ok(Target { dir: d.dir.clone(), device: i.device, display, pids: app_pids(&dump, &i.package, i.user.unwrap_or(0)) })
}

/// The pages the instance's app shows (its processes' DevTools sockets).
fn pages(t: &Target) -> Vec<Page> {
    let mut out = Vec::new();
    for name in omni_linux::xsocket::published(&t.dir) {
        let Some(pid) = String::from_utf8_lossy(&name).strip_prefix(DEVTOOLS).and_then(|p| p.parse::<u32>().ok()) else { continue };
        if !t.pids.contains(&pid) {
            continue;
        }
        let Ok(body) = http_get(&t.dir, &name, "/json") else { continue };
        let Ok(Json::Array(list)) = json::parse(&body) else { continue };
        for p in list {
            let s = |k: &str| p.get(k).and_then(Json::as_str).unwrap_or_default().to_string();
            if s("type") != "page" {
                continue;
            }
            let desc = json::parse(&s("description")).unwrap_or(Json::Null);
            let f = |k: &str| desc.get(k).and_then(Json::as_f64).unwrap_or(0.0);
            out.push(Page {
                socket: name.clone(),
                id: s("id"),
                url: s("url"),
                title: s("title"),
                visible: desc.get("visible").and_then(Json::as_bool).unwrap_or(false) && desc.get("attached").and_then(Json::as_bool).unwrap_or(false),
                screen: [f("screenX"), f("screenY"), f("width"), f("height")],
            });
        }
    }
    out
}

/// The page asked for (an id or its start), else the visible one (a real page before a blank one).
fn page(t: &Target, args: &Json) -> Result<Page, RpcError> {
    let all = pages(t);
    if all.is_empty() {
        return Err(RpcError::server("the instance shows no web page (no WebView, or its app has not made one yet)"));
    }
    if let Some(want) = arg_str(args, "page") {
        return all.into_iter().find(|p| p.id.starts_with(want)).ok_or_else(|| RpcError::params(format!("no page {want} (web_pages lists them)")));
    }
    let rank = |p: &Page| (p.visible, p.url != "about:blank");
    let mut all = all;
    all.sort_by_key(|p| std::cmp::Reverse(rank(p)));
    Ok(all.swap_remove(0))
}

/// How the page's CSS pixels lie on the display.
struct Geometry {
    /// The WebView's top-left on the display (device pixels).
    origin: (f64, f64),
    /// Device pixels per CSS pixel (zoom included).
    scale: f64,
    /// The CSS viewport's size.
    viewport: (f64, f64),
}

impl Geometry {
    fn of(cdp: &mut Cdp, page: &Page) -> Result<Self, String> {
        let m = cdp.call("Page.getLayoutMetrics", Json::Object(Default::default()))?;
        let vv = m.get("cssVisualViewport").or_else(|| m.get("visualViewport")).cloned().unwrap_or(Json::Null);
        let f = |k: &str| vv.get(k).and_then(Json::as_f64).unwrap_or(0.0);
        let width = f("clientWidth").max(1.0);
        let scale = if page.screen[2] > 0.0 { page.screen[2] / width } else { 1.0 };
        Ok(Self { origin: (page.screen[0], page.screen[1]), scale, viewport: (width, f("clientHeight")) })
    }

    /// The display point of the viewport's CSS point.
    fn screen(&self, x: f64, y: f64) -> (f64, f64) {
        (self.origin.0 + x * self.scale, self.origin.1 + y * self.scale)
    }

    /// The viewport's CSS point of a display point.
    fn css(&self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.origin.0) / self.scale, (y - self.origin.1) / self.scale)
    }

    fn in_view(&self, x: f64, y: f64) -> bool {
        x >= 0.0 && y >= 0.0 && x < self.viewport.0 && y < self.viewport.1
    }
}

// ---------------------------------------------------------------------------------------------
// The DOM.

/// An element of the document tree, and the url of the frame it is in.
struct Element<'a> {
    node: &'a Json,
    frame: String,
}

fn attrs(node: &Json) -> Vec<(String, String)> {
    let list = node.get("attributes").and_then(Json::as_array).unwrap_or_default();
    list.chunks(2).filter_map(|kv| Some((kv.first()?.as_str()?.to_string(), kv.get(1)?.as_str()?.to_string()))).collect()
}

fn tag(node: &Json) -> String {
    node.get("localName").and_then(Json::as_str).unwrap_or_default().to_string()
}

/// What can be clicked or typed into.
fn interactive(node: &Json) -> bool {
    let t = tag(node);
    matches!(t.as_str(), "a" | "button" | "input" | "select" | "textarea" | "label" | "summary" | "option" | "iframe" | "details")
        || attrs(node).iter().any(|(k, v)| matches!(k.as_str(), "onclick" | "tabindex" | "contenteditable") || (k == "role" && !v.is_empty()))
}

/// The element's text: its descendants' text nodes, spaces collapsed, cut at `max` characters.
fn text_of(node: &Json, max: usize) -> String {
    fn walk(n: &Json, out: &mut String, max: usize) {
        if out.len() >= max {
            return;
        }
        if n.get("nodeType").and_then(Json::as_u64) == Some(3) {
            out.push_str(n.get("nodeValue").and_then(Json::as_str).unwrap_or_default());
            out.push(' ');
        }
        if matches!(tag(n).as_str(), "script" | "style" | "noscript" | "template") {
            return;
        }
        for c in n.get("children").and_then(Json::as_array).unwrap_or_default() {
            walk(c, out, max);
        }
    }
    let mut s = String::new();
    walk(node, &mut s, max * 4);
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > max { format!("{}...", s.chars().take(max).collect::<String>()) } else { s }
}

/// The element's own text (its text children only).
fn own_text(node: &Json) -> String {
    let kids = node.get("children").and_then(Json::as_array).unwrap_or_default();
    let s: String = kids.iter().filter(|c| c.get("nodeType").and_then(Json::as_u64) == Some(3)).filter_map(|c| c.get("nodeValue").and_then(Json::as_str)).collect::<Vec<_>>().join(" ");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every element of the tree in document order (frames' documents too), and every document's
/// node id with its url.
fn flatten<'a>(root: &'a Json) -> (Vec<Element<'a>>, Vec<(u64, String)>) {
    fn walk<'a>(n: &'a Json, frame: &str, out: &mut Vec<Element<'a>>, docs: &mut Vec<(u64, String)>) {
        let frame = match n.get("documentURL").and_then(Json::as_str) {
            Some(url) if n.get("nodeType").and_then(Json::as_u64) == Some(9) => {
                if let Some(id) = n.get("nodeId").and_then(Json::as_u64) {
                    docs.push((id, url.to_string()));
                }
                url.to_string()
            }
            _ => frame.to_string(),
        };
        if n.get("nodeType").and_then(Json::as_u64) == Some(1) {
            out.push(Element { node: n, frame: frame.clone() });
        }
        for key in ["children", "shadowRoots"] {
            for c in n.get(key).and_then(Json::as_array).unwrap_or_default() {
                walk(c, &frame, out, docs);
            }
        }
        if let Some(d) = n.get("contentDocument") {
            walk(d, &frame, out, docs);
        }
    }
    let (mut out, mut docs) = (Vec::new(), Vec::new());
    walk(root, "", &mut out, &mut docs);
    (out, docs)
}

fn document(cdp: &mut Cdp) -> Result<Json, String> {
    cdp.call("DOM.getDocument", json::obj([("depth", Json::Num(-1.0)), ("pierce", Json::Bool(true))])).map(|r| r.get("root").cloned().unwrap_or(Json::Null))
}

/// An element's box (CSS pixels of the viewport: x, y, width, height), if it is laid out.
fn box_of(cdp: &mut Cdp, by: (&'static str, u64)) -> Option<[f64; 4]> {
    let m = cdp.call("DOM.getBoxModel", json::obj([(by.0, Json::Num(by.1 as f64))])).ok()?;
    let q: Vec<f64> = m.get("model")?.get("border")?.as_array()?.iter().filter_map(Json::as_f64).collect();
    if q.len() < 8 {
        return None;
    }
    let (xs, ys) = ([q[0], q[2], q[4], q[6]], [q[1], q[3], q[5], q[7]]);
    let (x0, y0) = (xs.iter().copied().fold(f64::MAX, f64::min), ys.iter().copied().fold(f64::MAX, f64::min));
    let (x1, y1) = (xs.iter().copied().fold(f64::MIN, f64::max), ys.iter().copied().fold(f64::MIN, f64::max));
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
}

/// The listing's entry for an element with box `b`.
fn entry(e: &Element<'_>, b: [f64; 4], geo: &Geometry, html: Option<String>) -> Json {
    const KEEP: &[&str] = &["id", "class", "name", "type", "role", "aria-label", "placeholder", "href", "value", "alt", "title", "src", "for", "data-testid"];
    let mut pairs = vec![
        ("ref".to_string(), Json::Num(e.node.get("backendNodeId").and_then(Json::as_f64).unwrap_or(0.0))),
        ("tag".to_string(), json::s(tag(e.node))),
    ];
    for (k, v) in attrs(e.node) {
        if KEEP.contains(&k.as_str()) {
            pairs.push((k, json::s(v.chars().take(160).collect::<String>())));
        }
    }
    let text = text_of(e.node, 100);
    if !text.is_empty() {
        pairs.push(("text".into(), json::s(text)));
    }
    let (cx, cy) = (b[0] + b[2] / 2.0, b[1] + b[3] / 2.0);
    let (sx, sy) = geo.screen(cx, cy);
    pairs.push(("box".into(), Json::Array(b.iter().map(|v| num(*v)).collect())));
    pairs.push(("tap".into(), Json::Array(vec![num(sx.round()), num(sy.round())])));
    pairs.push(("in_view".into(), Json::Bool(geo.in_view(cx, cy))));
    pairs.push(("frame".into(), json::s(e.frame.clone())));
    if let Some(h) = html {
        pairs.push(("html".into(), json::s(h)));
    }
    object(pairs)
}

/// The nodes `selector` matches, every frame's document searched in turn.
fn select(cdp: &mut Cdp, docs: &[(u64, String)], selector: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for (doc, url) in docs {
        let Ok(r) = cdp.call("DOM.querySelectorAll", json::obj([("nodeId", Json::Num(*doc as f64)), ("selector", json::s(selector))])) else { continue };
        out.extend(r.get("nodeIds").and_then(Json::as_array).unwrap_or_default().iter().filter_map(Json::as_u64).map(|id| (id, url.clone())));
    }
    out
}

/// The element a click names: its `backendNodeId` and a description. Of several that match, the
/// `index`th of those a person could click -- laid out, and on the page rather than parked far
/// off it as hidden widgets are (a captcha's unused challenge frame at -9999) -- those in view first.
fn resolve(cdp: &mut Cdp, page: &Page, args: &Json) -> Result<(u64, String), String> {
    if let Some(r) = arg_f64(args, "ref") {
        return Ok((r as u64, format!("ref {r}")));
    }
    let root = document(cdp)?;
    let (elements, docs) = flatten(&root);
    let index = arg_f64(args, "index").unwrap_or(0.0) as usize;
    let backend = |e: &Element<'_>| e.node.get("backendNodeId").and_then(Json::as_u64).unwrap_or(0);
    let candidates: Vec<(u64, String)> = if let Some(sel) = arg_str(args, "selector") {
        let ids = select(cdp, &docs, sel);
        ids.iter()
            .filter_map(|(id, _)| elements.iter().find(|e| e.node.get("nodeId").and_then(Json::as_u64) == Some(*id)))
            .map(|e| (backend(e), format!("<{}> `{sel}`", tag(e.node))))
            .collect()
    } else if let Some(want) = arg_str(args, "text") {
        let want = want.to_lowercase();
        // The innermost elements showing it: their own text first, then any interactive element's.
        let own: Vec<&Element<'_>> = elements.iter().filter(|e| own_text(e.node).to_lowercase().contains(&want)).collect();
        let any: Vec<&Element<'_>> = elements.iter().filter(|e| interactive(e.node) && text_of(e.node, 400).to_lowercase().contains(&want)).collect();
        (if own.is_empty() { any } else { own }).iter().map(|e| (backend(e), format!("<{}> \"{}\"", tag(e.node), text_of(e.node, 60)))).collect()
    } else {
        return Err("name the element: ref, selector or text".into());
    };
    if candidates.is_empty() {
        return Err("no element matches".into());
    }
    let geo = Geometry::of(cdp, page)?;
    let mut ranked: Vec<(u8, (u64, String))> = Vec::new();
    for c in candidates {
        let Some(b) = box_of(cdp, ("backendNodeId", c.0)) else { continue };
        let (cx, cy) = (b[0] + b[2] / 2.0, b[1] + b[3] / 2.0);
        if cx < -1000.0 || cy < -1000.0 {
            continue;
        }
        ranked.push((u8::from(!geo.in_view(cx, cy)), c));
    }
    ranked.sort_by_key(|(rank, _)| *rank);
    let n = ranked.len();
    ranked.into_iter().nth(index).map(|(_, c)| c).ok_or_else(|| if n == 0 { "every match is hidden (not laid out, or off the page)".to_string() } else { format!("{n} visible matches; no index {index}") })
}

/// Click the viewport's CSS point (x, y): a tap on the display, or DevTools mouse events.
fn click_at(pool: &Pool, t: &Target, cdp: &mut Cdp, geo: &Geometry, (x, y): (f64, f64), via: &str) -> Result<(f64, f64), String> {
    let (sx, sy) = geo.screen(x, y);
    if via == "cdp" {
        for (kind, buttons) in [("mouseMoved", 0.0), ("mousePressed", 1.0), ("mouseReleased", 0.0)] {
            cdp.call("Input.dispatchMouseEvent", json::obj([("type", json::s(kind)), ("x", num(x)), ("y", num(y)), ("button", json::s(if kind == "mouseMoved" { "none" } else { "left" })), ("buttons", Json::Num(buttons)), ("clickCount", Json::Num(1.0))]))?;
        }
    } else {
        let dev = pool.device(t.device).ok_or("its Android is gone")?;
        let (code, out) = dev.shell(&format!("input -d {} tap {} {}", t.display, sx.round(), sy.round()), Duration::from_secs(30))?;
        if code != 0 {
            return Err(format!("input tap: {}", out.trim()));
        }
    }
    Ok((sx.round(), sy.round()))
}

/// Click the element `id` (a backend node): scrolled into view, at its box's centre.
fn click_node(pool: &Pool, t: &Target, cdp: &mut Cdp, page: &Page, id: u64, via: &str) -> Result<((f64, f64), [f64; 4]), String> {
    let _ = cdp.call("DOM.scrollIntoViewIfNeeded", json::obj([("backendNodeId", Json::Num(id as f64))]));
    let b = box_of(cdp, ("backendNodeId", id)).ok_or("the element is not laid out (hidden, or gone)")?;
    let geo = Geometry::of(cdp, page)?;
    let at = click_at(pool, t, cdp, &geo, (b[0] + b[2] / 2.0, b[1] + b[3] / 2.0), via)?;
    Ok((at, b))
}

/// The default execution contexts of the frames whose url contains `part`, with their urls.
fn frame_contexts(cdp: &mut Cdp, part: &str) -> Result<Vec<(u64, String)>, String> {
    fn frames(n: &Json, out: &mut Vec<(String, String)>) {
        if let Some(f) = n.get("frame") {
            out.push((f.get("id").and_then(Json::as_str).unwrap_or_default().into(), f.get("url").and_then(Json::as_str).unwrap_or_default().into()));
        }
        for c in n.get("childFrames").and_then(Json::as_array).unwrap_or_default() {
            frames(c, out);
        }
    }
    let tree = cdp.call("Page.getFrameTree", Json::Object(Default::default()))?;
    let mut all = Vec::new();
    frames(tree.get("frameTree").unwrap_or(&Json::Null), &mut all);
    let wanted: Vec<&(String, String)> = all.iter().filter(|(_, url)| url.contains(part)).collect();
    if wanted.is_empty() {
        return Err(format!("no frame's url contains `{part}`: {}", all.iter().map(|(_, u)| u.as_str()).collect::<Vec<_>>().join(", ")));
    }
    cdp.events.clear();
    cdp.call("Runtime.enable", Json::Object(Default::default()))?;
    let contexts: Vec<(String, u64)> = cdp
        .events
        .iter()
        .filter(|e| e.get("method").and_then(Json::as_str) == Some("Runtime.executionContextCreated"))
        .filter_map(|e| {
            let c = e.get("params")?.get("context")?;
            let aux = c.get("auxData")?;
            (aux.get("isDefault").and_then(Json::as_bool) != Some(false)).then_some(())?;
            Some((aux.get("frameId")?.as_str()?.to_string(), c.get("id")?.as_u64()?))
        })
        .collect();
    let _ = cdp.call("Runtime.disable", Json::Object(Default::default()));
    let found: Vec<(u64, String)> = wanted.iter().filter_map(|(frame, url)| contexts.iter().find(|(f, _)| f == frame).map(|(_, id)| (*id, url.clone()))).collect();
    if found.is_empty() {
        return Err(format!("the frames matching `{part}` have no script context yet"));
    }
    Ok(found)
}

// ---------------------------------------------------------------------------------------------
// The tools.

/// Run web tool `name`; `None` when it is not one.
pub fn call(pool: &Pool, name: &str, args: &Json) -> Option<Result<Json, RpcError>> {
    TOOLS.iter().any(|t| t.0 == name).then(|| run(pool, name, args))
}

fn run(pool: &Pool, name: &str, args: &Json) -> Result<Json, RpcError> {
    let id = args.get("instance_id").and_then(Json::as_u64).ok_or_else(|| RpcError::params("need `instance_id`"))? as u32;
    let t = target(pool, id)?;
    if name == "web_pages" {
        let list = pages(&t);
        return Ok(json::obj([(
            "pages",
            Json::Array(
                list.iter()
                    .map(|p| {
                        json::obj([
                            ("page", json::s(p.id.clone())),
                            ("url", json::s(p.url.clone())),
                            ("title", json::s(p.title.clone())),
                            ("visible", Json::Bool(p.visible)),
                            ("screen", Json::Array(p.screen.iter().map(|v| num(*v)).collect())),
                        ])
                    })
                    .collect(),
            ),
        )]));
    }
    let page = page(&t, args)?;
    let mut cdp = Cdp::open(&t.dir, &page.socket, &page.id).map_err(err)?;
    let about = |extra: Vec<(String, Json)>| {
        let mut pairs = vec![("page".to_string(), json::s(page.id.clone())), ("url".to_string(), json::s(page.url.clone()))];
        pairs.extend(extra);
        object(pairs)
    };
    match name {
        "web_dom" => {
            let geo = Geometry::of(&mut cdp, &page).map_err(err)?;
            let root = document(&mut cdp).map_err(err)?;
            let (elements, docs) = flatten(&root);
            let limit = arg_f64(args, "limit").unwrap_or(200.0).clamp(1.0, 2000.0) as usize;
            let mut out = Vec::new();
            if let Some(sel) = arg_str(args, "selector") {
                for (node, _) in select(&mut cdp, &docs, sel).into_iter().take(limit) {
                    let Some(e) = elements.iter().find(|e| e.node.get("nodeId").and_then(Json::as_u64) == Some(node)) else { continue };
                    let html = arg_bool(args, "html").then(|| {
                        cdp.call("DOM.getOuterHTML", json::obj([("nodeId", Json::Num(node as f64))])).ok().and_then(|r| r.get("outerHTML").and_then(Json::as_str).map(|h| h.chars().take(4000).collect()))
                    });
                    match box_of(&mut cdp, ("nodeId", node)) {
                        Some(b) => out.push(entry(e, b, &geo, html.flatten())),
                        None => out.push(object(vec![("ref".into(), Json::Num(e.node.get("backendNodeId").and_then(Json::as_f64).unwrap_or(0.0))), ("tag".into(), json::s(tag(e.node))), ("laid_out".into(), Json::Bool(false))])),
                    }
                }
            } else {
                let wanted = |e: &Element<'_>| {
                    let t = tag(e.node);
                    let text = own_text(e.node);
                    interactive(e.node) || matches!(t.as_str(), "h1" | "h2" | "h3" | "h4" | "img") || (!text.is_empty() && text.len() <= 200 && !matches!(t.as_str(), "script" | "style" | "noscript" | "title" | "head" | "html" | "body"))
                };
                for e in elements.iter().filter(|e| wanted(e)) {
                    if out.len() >= limit {
                        break;
                    }
                    let Some(node) = e.node.get("nodeId").and_then(Json::as_u64) else { continue };
                    if let Some(b) = box_of(&mut cdp, ("nodeId", node)) {
                        out.push(entry(e, b, &geo, None));
                    }
                }
            }
            Ok(about(vec![
                ("viewport".into(), Json::Array(vec![num(geo.viewport.0), num(geo.viewport.1)])),
                ("device_px_per_css_px".into(), num(geo.scale)),
                ("count".into(), Json::Num(out.len() as f64)),
                ("elements".into(), Json::Array(out)),
            ]))
        }
        "web_eval" => {
            let expression = arg_str(args, "expression").ok_or_else(|| RpcError::params("need `expression`"))?;
            let eval = |cdp: &mut Cdp, context: Option<u64>| -> Result<Json, String> {
                let mut params = vec![
                    ("expression".to_string(), json::s(expression)),
                    ("returnByValue".to_string(), Json::Bool(true)),
                    ("awaitPromise".to_string(), Json::Bool(true)),
                    ("userGesture".to_string(), Json::Bool(true)),
                ];
                if let Some(id) = context {
                    params.push(("contextId".into(), Json::Num(id as f64)));
                }
                let r = cdp.call("Runtime.evaluate", object(params))?;
                if let Some(x) = r.get("exceptionDetails") {
                    let what = x.get("exception").and_then(|e| e.get("description")).and_then(Json::as_str).or_else(|| x.get("text").and_then(Json::as_str)).unwrap_or("an exception");
                    return Err(format!("the script threw: {what}"));
                }
                let v = r.get("result").cloned().unwrap_or(Json::Null);
                Ok(v.get("value").cloned().unwrap_or_else(|| json::s(v.get("description").and_then(Json::as_str).unwrap_or("undefined"))))
            };
            let Some(part) = arg_str(args, "frame") else {
                return Ok(about(vec![("value".into(), eval(&mut cdp, None).map_err(err)?)]));
            };
            // Every frame that matches (a captcha has two: its checkbox and its challenge).
            let frames = frame_contexts(&mut cdp, part).map_err(err)?;
            let mut answers = Vec::new();
            for (id, url) in frames {
                let answer = match eval(&mut cdp, Some(id)) {
                    Ok(v) => ("value", v),
                    Err(e) => ("error", json::s(e)),
                };
                answers.push(json::obj([("frame", json::s(url)), answer]));
            }
            Ok(about(vec![("frames".into(), Json::Array(answers))]))
        }
        "web_click" => {
            let (node, what) = resolve(&mut cdp, &page, args).map_err(err)?;
            let via = arg_str(args, "via").unwrap_or("touch");
            let ((sx, sy), b) = click_node(pool, &t, &mut cdp, &page, node, via).map_err(err)?;
            Ok(about(vec![
                ("clicked".into(), json::s(what)),
                ("ref".into(), Json::Num(node as f64)),
                ("box".into(), Json::Array(b.iter().map(|v| num(*v)).collect())),
                ("tap".into(), Json::Array(vec![num(sx), num(sy)])),
                ("via".into(), json::s(via)),
            ]))
        }
        "web_tap" => {
            let (x, y) = (arg_f64(args, "x").ok_or_else(|| RpcError::params("need `x`"))?, arg_f64(args, "y").ok_or_else(|| RpcError::params("need `y`"))?);
            let geo = Geometry::of(&mut cdp, &page).map_err(err)?;
            let css = if arg_str(args, "space") == Some("screen") { geo.css(x, y) } else { (x, y) };
            let via = arg_str(args, "via").unwrap_or("touch");
            let (sx, sy) = click_at(pool, &t, &mut cdp, &geo, css, via).map_err(err)?;
            Ok(about(vec![("css".into(), Json::Array(vec![num(css.0), num(css.1)])), ("tap".into(), Json::Array(vec![num(sx), num(sy)])), ("via".into(), json::s(via))]))
        }
        "web_type" => {
            let text = arg_str(args, "text").ok_or_else(|| RpcError::params("need `text`"))?;
            let mut clicked = Json::Null;
            // The element by ref or selector only: `text` here is what is typed.
            let named: Vec<(String, Json)> = ["ref", "selector", "index"].iter().filter_map(|k| Some(((*k).to_string(), args.get(k)?.clone()))).collect();
            if named.iter().any(|(k, _)| k != "index") {
                let (node, what) = resolve(&mut cdp, &page, &object(named)).map_err(err)?;
                click_node(pool, &t, &mut cdp, &page, node, "touch").map_err(err)?;
                let _ = cdp.call("DOM.focus", json::obj([("backendNodeId", Json::Num(node as f64))]));
                clicked = json::s(what);
            }
            cdp.call("Input.insertText", json::obj([("text", json::s(text))])).map_err(err)?;
            if arg_bool(args, "submit") {
                for kind in ["keyDown", "keyUp"] {
                    cdp.call("Input.dispatchKeyEvent", json::obj([("type", json::s(kind)), ("key", json::s("Enter")), ("code", json::s("Enter")), ("windowsVirtualKeyCode", Json::Num(13.0)), ("text", json::s(if kind == "keyDown" { "\r" } else { "" }))])).map_err(err)?;
                }
            }
            Ok(about(vec![("typed".into(), json::s(text)), ("into".into(), clicked)]))
        }
        "web_screenshot" => {
            let r = cdp.call("Page.captureScreenshot", json::obj([("format", json::s("png"))])).map_err(err)?;
            let data = r.get("data").and_then(Json::as_str).ok_or_else(|| RpcError::server("no image"))?;
            let bytes = base64(data).ok_or_else(|| RpcError::server("the image is not base64"))?;
            let path = t.dir.with_extension(format!("web{id}.png"));
            std::fs::write(&path, &bytes).map_err(err)?;
            let size = |at: usize| bytes.get(at..at + 4).map_or(0, |b| u32::from_be_bytes(b.try_into().expect("4")));
            Ok(about(vec![("path".into(), json::s(path.display().to_string())), ("width".into(), Json::Num(f64::from(size(16)))), ("height".into(), Json::Num(f64::from(size(20))))]))
        }
        other => Err(RpcError::params(format!("no tool {other}"))),
    }
}

/// Standard base64 (padding and line breaks allowed).
fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_s_processes_are_read_from_the_activity_manager() {
        let dump = "  *APP* UID 1010087 ProcessRecord{8cdebf1 416000:org.chromium.webview_shell/u10a87}\n\
                    *APP* UID 1099002 ProcessRecord{194ab4b 424000:com.android.webview:sandboxed_process0:org.chromium.content.app.SandboxedProcessService0:0/u10a87i2}\n\
                    *APP* UID 1110087 ProcessRecord{1 516000:org.chromium.webview_shell/u11a87}\n\
                    *APP* UID 1010087 ProcessRecord{2 417000:org.chromium.webview_shell:extra/u10a87}\n";
        assert_eq!(app_pids(dump, "org.chromium.webview_shell", 10), [416000, 417000]);
        assert_eq!(app_pids(dump, "org.chromium.webview_shell", 11), [516000]);
    }

    #[test]
    fn base64_decodes() {
        assert_eq!(base64("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64("iVBORw0KGgo=").as_deref(), Some(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10][..]));
    }

    #[test]
    fn a_tree_flattens_through_frames() {
        let root = json::parse(r#"{"nodeId":1,"nodeType":9,"documentURL":"https://a/","children":[{"nodeId":2,"nodeType":1,"localName":"html","backendNodeId":20,"children":[{"nodeId":3,"nodeType":1,"localName":"iframe","backendNodeId":30,"attributes":["src","https://b/"],"contentDocument":{"nodeId":4,"nodeType":9,"documentURL":"https://b/","children":[{"nodeId":5,"nodeType":1,"localName":"button","backendNodeId":50,"children":[{"nodeId":6,"nodeType":3,"nodeValue":" I am  human "}]}]}}]}]}"#).unwrap();
        let (elements, docs) = flatten(&root);
        assert_eq!(docs, [(1, "https://a/".to_string()), (4, "https://b/".to_string())]);
        let tags: Vec<(String, String)> = elements.iter().map(|e| (tag(e.node), e.frame.clone())).collect();
        assert_eq!(tags, [("html".into(), "https://a/".into()), ("iframe".into(), "https://a/".into()), ("button".into(), "https://b/".into())]);
        assert_eq!(text_of(elements[2].node, 100), "I am human");
        assert!(interactive(elements[2].node) && interactive(elements[1].node) && !interactive(elements[0].node));
    }
}
