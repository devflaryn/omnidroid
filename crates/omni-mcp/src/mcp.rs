//! The MCP transport: JSON-RPC 2.0 over stdio, one message per line.
//!
//! The [Model Context Protocol](https://modelcontextprotocol.io) stdio transport frames each message
//! as a single line of UTF-8 JSON (no embedded newlines). This module reads requests, dispatches
//! them to a [`Dispatch`], and writes responses; it knows nothing about the tools themselves.

use std::io::{BufRead, Write};

use crate::json::{self, Json};

/// The JSON-RPC 2.0 error codes this server uses.
pub mod code {
    /// Invalid JSON was received.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON is not a valid request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameters.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal error.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// A tool or server operation failed (application-level).
    pub const SERVER_ERROR: i64 = -32000;
}

/// A JSON-RPC error to return for a request.
#[derive(Debug, Clone)]
pub struct RpcError {
    /// One of [`code`].
    pub code: i64,
    /// Human-readable message.
    pub message: String,
}

impl RpcError {
    /// A server-error (`-32000`) with a message.
    pub fn server(message: impl Into<String>) -> Self {
        Self { code: code::SERVER_ERROR, message: message.into() }
    }
    /// An invalid-params (`-32602`) with a message.
    pub fn params(message: impl Into<String>) -> Self {
        Self { code: code::INVALID_PARAMS, message: message.into() }
    }
}

/// What a server must implement to be driven over this transport.
pub trait Dispatch {
    /// Handle one request `method` with `params` (which may be [`Json::Null`]).
    ///
    /// Returns the `result` value on success, or an [`RpcError`] on failure. Notifications (requests
    /// with no id) are dispatched too; their return value is discarded.
    fn handle(&mut self, method: &str, params: &Json) -> Result<Json, RpcError>;
}

/// A parsed request.
struct Request {
    /// The `id`, verbatim, or `None` for a notification.
    id: Option<Json>,
    method: String,
    params: Json,
}

fn parse_request(line: &str) -> Result<Request, RpcError> {
    let value = json::parse(line)
        .map_err(|e| RpcError { code: code::PARSE_ERROR, message: e.to_string() })?;
    let obj = value
        .as_object()
        .ok_or_else(|| RpcError { code: code::INVALID_REQUEST, message: "not an object".into() })?;
    let method = obj
        .get("method")
        .and_then(Json::as_str)
        .ok_or_else(|| RpcError {
            code: code::INVALID_REQUEST,
            message: "missing method".into(),
        })?
        .to_string();
    let id = obj.get("id").cloned();
    let params = obj.get("params").cloned().unwrap_or(Json::Null);
    Ok(Request { id, method, params })
}

fn response(id: Json, result: Json) -> String {
    json::obj([
        ("jsonrpc", json::s("2.0")),
        ("id", id),
        ("result", result),
    ])
    .to_string()
}

fn error_response(id: Json, err: &RpcError) -> String {
    json::obj([
        ("jsonrpc", json::s("2.0")),
        ("id", id),
        (
            "error",
            json::obj([("code", Json::Num(err.code as f64)), ("message", json::s(err.message.clone()))]),
        ),
    ])
    .to_string()
}

/// Run the stdio serve loop against `dispatch`, reading requests from `reader` and writing responses
/// to `writer`, one JSON message per line, until EOF.
///
/// # Errors
///
/// Propagates an I/O error from reading or writing the streams. A malformed or failing *request* is
/// answered with a JSON-RPC error, not returned as an error here.
pub fn serve(
    mut reader: impl BufRead,
    mut writer: impl Write,
    dispatch: &mut impl Dispatch,
) -> std::io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(()); // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_request(trimmed) {
            Ok(req) => {
                let is_notification = req.id.is_none();
                let outcome = dispatch.handle(&req.method, &req.params);
                if is_notification {
                    // Notifications get no response, even on error.
                    continue;
                }
                let id = req.id.unwrap_or(Json::Null);
                let text = match outcome {
                    Ok(result) => response(id, result),
                    Err(err) => error_response(id, &err),
                };
                writeln!(writer, "{text}")?;
                writer.flush()?;
            }
            Err(err) => {
                // Could not even parse: reply with a null id (JSON-RPC allows this for parse errors).
                let text = error_response(Json::Null, &err);
                writeln!(writer, "{text}")?;
                writer.flush()?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Echo;
    impl Dispatch for Echo {
        fn handle(&mut self, method: &str, params: &Json) -> Result<Json, RpcError> {
            match method {
                "ping" => Ok(Json::Object(Default::default())),
                "echo" => Ok(params.clone()),
                "boom" => Err(RpcError::server("kaboom")),
                _ => Err(RpcError { code: code::METHOD_NOT_FOUND, message: method.into() }),
            }
        }
    }

    fn run(input: &str) -> String {
        let mut out = Vec::new();
        serve(Cursor::new(input.as_bytes()), &mut out, &mut Echo).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn answers_a_request_with_its_id() {
        let out = run("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n");
        let v = json::parse(out.trim()).unwrap();
        assert_eq!(v.get("id").unwrap().as_f64(), Some(1.0));
        assert!(v.get("result").is_some());
    }

    #[test]
    fn echoes_params() {
        let out = run("{\"jsonrpc\":\"2.0\",\"id\":\"a\",\"method\":\"echo\",\"params\":{\"x\":5}}\n");
        let v = json::parse(out.trim()).unwrap();
        assert_eq!(v.get("result").unwrap().get("x").unwrap().as_f64(), Some(5.0));
    }

    #[test]
    fn a_notification_gets_no_reply() {
        // No id => notification => no output.
        let out = run("{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n");
        assert!(out.is_empty(), "a notification must produce no response, got {out:?}");
    }

    #[test]
    fn reports_errors_as_json_rpc_errors() {
        let out = run("{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"boom\"}\n");
        let v = json::parse(out.trim()).unwrap();
        assert_eq!(v.get("error").unwrap().get("code").unwrap().as_f64(), Some(-32000.0));
        let out = run("{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"nope\"}\n");
        let v = json::parse(out.trim()).unwrap();
        assert_eq!(v.get("error").unwrap().get("code").unwrap().as_f64(), Some(-32601.0));
    }

    #[test]
    fn malformed_json_gets_a_parse_error() {
        let out = run("{not json\n");
        let v = json::parse(out.trim()).unwrap();
        assert_eq!(v.get("error").unwrap().get("code").unwrap().as_f64(), Some(-32700.0));
    }

    #[test]
    fn handles_several_messages_in_sequence() {
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\
                     {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"echo\",\"params\":[1,2]}\n";
        let out = run(input);
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 2, "one response per request");
    }
}
