//! **MCP over HTTP** (the specification's Streamable HTTP transport, 2025-03-26, without server-sent
//! events): one endpoint, `POST /mcp`, a JSON-RPC message in the body and its answer as
//! `application/json` -- so any number of agents reach one server, the app's, at once. A
//! notification is answered `202 Accepted`; `GET` (an event stream) is `405`.
//!
//! **This computer only**: it listens on 127.0.0.1, and a request whose `Origin` names another host
//! is refused (`403`), as the transport asks of a local server against DNS rebinding.
//!
//! [`bridge`] is the other end for an agent that speaks MCP only on stdio: each line it writes is
//! posted, each answer written back (`omni-mcp --connect <url>`).
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::mcp::{handle_message, Dispatch};

/// The path the server answers on.
pub const PATH: &str = "/mcp";
/// The largest body taken (a JSON-RPC message, not a file upload).
const MAX_BODY: usize = 16 << 20;

/// A running server: where it listens, and how to stop it.
pub struct Listening {
    pub port: u16,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Listening {
    /// The URL an agent is given.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}{PATH}", self.port)
    }

    /// Stop taking connections (the ones open finish their request).
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        // Wake the accept loop.
        let _ = TcpStream::connect_timeout(&format!("127.0.0.1:{}", self.port).parse().expect("an address"), Duration::from_millis(200));
    }
}

/// Serve `dispatch` on 127.0.0.1:`port` (0: any free port), a thread per connection.
///
/// # Errors
/// The port could not be taken.
pub fn serve<D: Dispatch + Send + 'static>(port: u16, dispatch: Arc<Mutex<D>>) -> std::io::Result<Listening> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let port = listener.local_addr()?.port();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    std::thread::Builder::new().name("omni-mcp-http".into()).spawn(move || {
        for stream in listener.incoming() {
            if stopping.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let dispatch = Arc::clone(&dispatch);
            let _ = std::thread::Builder::new().name("omni-mcp-conn".into()).spawn(move || connection(stream, &dispatch));
        }
    })?;
    Ok(Listening { port, stop })
}

/// One request's head.
#[derive(Debug, Default, PartialEq, Eq)]
struct Head {
    method: String,
    path: String,
    length: usize,
    origin: Option<String>,
    close: bool,
}

fn read_head(reader: &mut impl BufRead) -> std::io::Result<Option<Head>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let mut head = Head { method: parts.next().unwrap_or_default().to_string(), path: parts.next().unwrap_or_default().to_string(), ..Head::default() };
    let http10 = parts.next() == Some("HTTP/1.0");
    head.close = http10;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let v = v.trim();
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => head.length = v.parse().unwrap_or(0),
                "origin" => head.origin = Some(v.to_string()),
                "connection" => head.close = v.eq_ignore_ascii_case("close"),
                _ => {}
            }
        }
    }
    Ok(Some(head))
}

/// Whether an `Origin` is this computer's (or absent: a program, not a page).
fn local_origin(origin: Option<&str>) -> bool {
    let Some(o) = origin else { return true };
    let host = o.split("://").nth(1).unwrap_or(o).split('/').next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host });
    matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "null")
}

fn answer(stream: &mut TcpStream, status: &str, body: &str, close: bool) -> std::io::Result<()> {
    let kind = if body.is_empty() { "" } else { "Content-Type: application/json\r\n" };
    write!(stream, "HTTP/1.1 {status}\r\n{kind}Content-Length: {}\r\nMcp-Session-Id: omnidroid\r\n{}\r\n{body}", body.len(), if close { "Connection: close\r\n" } else { "" })?;
    stream.flush()
}

fn connection<D: Dispatch>(stream: TcpStream, dispatch: &Mutex<D>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(600)));
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    while let Ok(Some(head)) = read_head(&mut reader) {
        let close = head.close;
        let result = if !local_origin(head.origin.as_deref()) {
            answer(&mut writer, "403 Forbidden", "", true).map(|()| true)
        } else if head.path.split('?').next() != Some(PATH) {
            answer(&mut writer, "404 Not Found", "", close).map(|()| close)
        } else if head.method == "POST" {
            if head.length > MAX_BODY {
                let _ = answer(&mut writer, "413 Payload Too Large", "", true);
                return;
            }
            let mut body = vec![0; head.length];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            let text = String::from_utf8_lossy(&body);
            match handle_message(&text, &mut *dispatch.lock()) {
                Some(out) => answer(&mut writer, "200 OK", &out, close).map(|()| close),
                None => answer(&mut writer, "202 Accepted", "", close).map(|()| close),
            }
        } else if head.method == "DELETE" {
            answer(&mut writer, "200 OK", "", close).map(|()| close)
        } else {
            answer(&mut writer, "405 Method Not Allowed", "", close).map(|()| close)
        };
        match result {
            Ok(false) => {}
            _ => return,
        }
    }
}

/// Post one message to `url` (`http://127.0.0.1:<port>/mcp`): the answer's body, empty for a
/// notification.
///
/// # Errors
/// Not an http URL, the server unreachable, or an answer that is not 200/202.
pub fn post(url: &str, body: &str) -> Result<String, String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| format!("not an http:// URL: {url}"))?;
    let (host, path) = rest.split_once('/').map_or((rest, "/"), |(h, p)| (h, p));
    let mut stream = TcpStream::connect(host).map_err(|e| format!("{host}: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(900)));
    write!(stream, "POST /{} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", path.trim_start_matches('/'), body.len())
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(|e| e.to_string())?;
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                length = v.trim().parse::<usize>().ok();
            }
        }
    }
    let mut out = Vec::new();
    match length {
        Some(n) => {
            out.resize(n, 0);
            reader.read_exact(&mut out).map_err(|e| e.to_string())?;
        }
        None => {
            reader.read_to_end(&mut out).map_err(|e| e.to_string())?;
        }
    }
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if code != "200" && code != "202" {
        return Err(format!("{url}: {}", status.trim()));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// **stdio <-> HTTP**: each line read from `input` posted to `url`, each answer written to
/// `output` -- an agent that starts MCP servers as programs reaches the app's server this way.
/// An unreachable server answers each request with a JSON-RPC error that says so.
///
/// # Errors
/// Reading `input` or writing `output` failed.
pub fn bridge(url: &str, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let answer = match post(url, &line) {
            Ok(body) => body,
            Err(e) => {
                let id = crate::json::parse(&line).ok().and_then(|v| v.get("id").cloned());
                match id {
                    Some(id) => crate::json::obj([
                        ("jsonrpc", crate::json::s("2.0")),
                        ("id", id),
                        ("error", crate::json::obj([("code", crate::json::Json::Num(-32000.0)), ("message", crate::json::s(format!("the omnidroid app's MCP server is not reachable ({e}): open the app and turn MCP on in its settings")))])),
                    ])
                    .to_string(),
                    None => String::new(),
                }
            }
        };
        if !answer.trim().is_empty() {
            writeln!(output, "{}", answer.trim())?;
            output.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{self, Json};
    use crate::mcp::RpcError;

    struct Echo;
    impl Dispatch for Echo {
        fn handle(&mut self, method: &str, params: &Json) -> Result<Json, RpcError> {
            match method {
                "echo" => Ok(params.clone()),
                _ => Ok(Json::Null),
            }
        }
    }

    #[test]
    fn a_request_is_answered_a_notification_accepted_and_a_foreign_page_refused() {
        let server = serve(0, Arc::new(Mutex::new(Echo))).expect("a port");
        let url = server.url();
        let out = post(&url, r#"{"jsonrpc":"2.0","id":7,"method":"echo","params":{"a":1}}"#).unwrap();
        let v = json::parse(&out).unwrap();
        assert_eq!(v.get("id").and_then(Json::as_f64), Some(7.0));
        assert_eq!(v.get("result").and_then(|r| r.get("a")).and_then(Json::as_f64), Some(1.0));
        assert_eq!(post(&url, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap(), "", "202, no body");
        let batch = post(&url, r#"[{"jsonrpc":"2.0","id":1,"method":"echo","params":1},{"jsonrpc":"2.0","method":"x"}]"#).unwrap();
        assert!(batch.starts_with('[') && batch.contains("\"id\":1"), "{batch}");
        assert!(post(&url.replace("/mcp", "/other"), "{}").is_err(), "404");
        // A page of another site (DNS rebinding) is refused.
        let mut s = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        write!(s, "POST /mcp HTTP/1.1\r\nOrigin: http://evil.example\r\nContent-Length: 2\r\n\r\n{{}}").unwrap();
        let mut status = String::new();
        BufReader::new(s).read_line(&mut status).unwrap();
        assert!(status.contains("403"), "{status}");
        assert!(local_origin(Some("http://localhost:3000")) && local_origin(None) && !local_origin(Some("https://127.0.0.1.evil.example")));
        server.stop();
    }

    #[test]
    fn the_bridge_carries_lines_both_ways_and_says_when_the_app_is_not_there() {
        let server = serve(0, Arc::new(Mutex::new(Echo))).expect("a port");
        let mut out = Vec::new();
        bridge(&server.url(), "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"echo\",\"params\":2}\n\n{\"jsonrpc\":\"2.0\",\"method\":\"n\"}\n".as_bytes(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1, "the notification is not answered: {text}");
        server.stop();
        let mut out = Vec::new();
        bridge("http://127.0.0.1:1/mcp", "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"x\"}\n".as_bytes(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"id\":3") && text.contains("not reachable"), "{text}");
    }
}
