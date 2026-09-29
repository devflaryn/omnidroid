//! A small, self-contained JSON value model, parser and serializer.
//!
//! MCP messages are JSON-RPC 2.0: objects, arrays, strings, numbers, booleans and null, and nothing
//! exotic. Rather than add serde to the workspace (the launcher hand-parses JSON too — main.rs,
//! "without a JSON crate"), this carries exactly enough JSON, correctly: full string escaping on the
//! way in and out, `\uXXXX` including surrogate pairs, and numbers kept as `f64` with integer-valued
//! ones printed without a fractional part.
//!
//! It is deliberately strict about structure and lenient about nothing: a malformed message from an
//! agent becomes a typed [`ParseError`] the server turns into a JSON-RPC error, never a panic.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A number, always held as `f64`. Integer-valued numbers serialize without a decimal point.
    Num(f64),
    /// A string, unescaped.
    Str(String),
    /// An array.
    Array(Vec<Json>),
    /// An object. `BTreeMap` so serialization is deterministic (stable key order), which makes tests
    /// and golden comparisons possible.
    Object(BTreeMap<String, Json>),
}

impl Json {
    /// Borrow this as an object's map, if it is one.
    #[must_use]
    pub fn as_object(&self) -> Option<&BTreeMap<String, Json>> {
        match self {
            Json::Object(m) => Some(m),
            _ => None,
        }
    }

    /// Borrow this as a string, if it is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// This as an `f64`, if it is a number.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// This as a `u64`, if it is a non-negative integer-valued number. Accepts values up to 2^53
    /// exactly (an f64 holds integers up to there without loss); a fractional or negative value is
    /// `None`. A string of digits is *also* accepted, so an agent may pass a 64-bit guest address as
    /// a string to avoid the 2^53 float limit — which is how addresses are passed on the wire.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) if n.fract() == 0.0 && *n >= 0.0 && *n <= 9_007_199_254_740_992.0 => {
                Some(*n as u64)
            }
            Json::Str(s) => parse_u64_flexible(s),
            _ => None,
        }
    }

    /// Borrow this as an array, if it is one.
    #[must_use]
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    /// This as a bool, if it is one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Look up a field of an object.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Json> {
        self.as_object().and_then(|m| m.get(key))
    }

    /// Serialize to a compact JSON string (no insignificant whitespace).
    #[must_use]
    pub fn to_string(&self) -> String {
        let mut out = String::new();
        self.write_to(&mut out);
        out
    }

    fn write_to(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Num(n) => {
                if n.is_finite() {
                    if n.fract() == 0.0 && n.abs() < 1e16 {
                        let _ = write!(out, "{}", *n as i64);
                    } else {
                        let _ = write!(out, "{n}");
                    }
                } else {
                    // JSON has no infinity/NaN; null is the conventional stand-in.
                    out.push_str("null");
                }
            }
            Json::Str(s) => write_escaped(out, s),
            Json::Array(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_to(out);
                }
                out.push(']');
            }
            Json::Object(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(out, k);
                    out.push(':');
                    v.write_to(out);
                }
                out.push('}');
            }
        }
    }
}

/// Convenience: build an object from pairs.
#[must_use]
pub fn obj(pairs: impl IntoIterator<Item = (&'static str, Json)>) -> Json {
    Json::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Convenience: a string value.
#[must_use]
pub fn s(text: impl Into<String>) -> Json {
    Json::Str(text.into())
}

fn parse_u64_flexible(text: &str) -> Option<u64> {
    let t = text.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u64>().ok()
    }
}

fn write_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A JSON parse failure, with a byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// What went wrong.
    pub message: String,
    /// Byte offset into the input where it was noticed.
    pub at: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON parse error at byte {}: {}", self.at, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parse a JSON document. Trailing whitespace is allowed; trailing non-whitespace is an error.
///
/// # Errors
///
/// [`ParseError`] for any malformed input.
pub fn parse(input: &str) -> Result<Json, ParseError> {
    let mut p = Parser { bytes: input.as_bytes(), pos: 0 };
    p.skip_ws();
    let value = p.value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.err("trailing data after the JSON value"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, message: &str) -> ParseError {
        ParseError { message: message.to_string(), at: self.pos }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn value(&mut self) -> Result<Json, ParseError> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') | Some(b'f') => self.boolean(),
            Some(b'n') => self.null(),
            Some(b'-') | Some(b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("expected a JSON value")),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn expect(&mut self, b: u8, what: &str) -> Result<(), ParseError> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(what))
        }
    }

    fn object(&mut self) -> Result<Json, ParseError> {
        self.expect(b'{', "expected '{'")?;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected a string key"));
            }
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':', "expected ':' after a key")?;
            self.skip_ws();
            let value = self.value()?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(map));
                }
                _ => return Err(self.err("expected ',' or '}' in an object")),
            }
        }
    }

    fn array(&mut self) -> Result<Json, ParseError> {
        self.expect(b'[', "expected '['")?;
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(out));
        }
        loop {
            self.skip_ws();
            out.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(out));
                }
                _ => return Err(self.err("expected ',' or ']' in an array")),
            }
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.expect(b'"', "expected '\"'")?;
        let mut out = String::new();
        loop {
            let b = self.peek().ok_or_else(|| self.err("unterminated string"))?;
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let cp = self.hex4()?;
                            if (0xD800..=0xDBFF).contains(&cp) {
                                // A high surrogate must be followed by \uXXXX low surrogate.
                                if self.peek() == Some(b'\\') {
                                    self.pos += 1;
                                    self.expect(b'u', "expected a low surrogate escape")?;
                                    let low = self.hex4()?;
                                    if !(0xDC00..=0xDFFF).contains(&low) {
                                        return Err(self.err("invalid low surrogate"));
                                    }
                                    let c = 0x1_0000
                                        + ((cp - 0xD800) << 10)
                                        + (low - 0xDC00);
                                    out.push(
                                        char::from_u32(c)
                                            .ok_or_else(|| self.err("invalid surrogate pair"))?,
                                    );
                                } else {
                                    return Err(self.err("lone high surrogate"));
                                }
                            } else if (0xDC00..=0xDFFF).contains(&cp) {
                                return Err(self.err("lone low surrogate"));
                            } else {
                                out.push(
                                    char::from_u32(cp).ok_or_else(|| self.err("invalid \\u"))?,
                                );
                            }
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                // A raw control character is invalid in a JSON string.
                0x00..=0x1F => return Err(self.err("control character in a string")),
                // Any other byte is part of a UTF-8 sequence; collect the whole char.
                _ => {
                    // Back up and decode one UTF-8 char from the input.
                    let start = self.pos - 1;
                    let len = utf8_len(b);
                    if len == 0 || start + len > self.bytes.len() {
                        return Err(self.err("invalid UTF-8 in a string"));
                    }
                    let slice = &self.bytes[start..start + len];
                    let text = std::str::from_utf8(slice)
                        .map_err(|_| self.err("invalid UTF-8 in a string"))?;
                    out.push_str(text);
                    self.pos = start + len;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.err("truncated \\u escape"));
        }
        let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| self.err("bad \\u escape"))?;
        let cp = u32::from_str_radix(hex, 16).map_err(|_| self.err("bad \\u hex"))?;
        self.pos += 4;
        Ok(cp)
    }

    fn boolean(&mut self) -> Result<Json, ParseError> {
        if self.bytes[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Ok(Json::Bool(true))
        } else if self.bytes[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Ok(Json::Bool(false))
        } else {
            Err(self.err("expected true or false"))
        }
    }

    fn null(&mut self) -> Result<Json, ParseError> {
        if self.bytes[self.pos..].starts_with(b"null") {
            self.pos += 4;
            Ok(Json::Null)
        } else {
            Err(self.err("expected null"))
        }
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("bad number"))?;
        let n: f64 = text.parse().map_err(|_| self.err("bad number"))?;
        Ok(Json::Num(n))
    }
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_scalars() {
        for text in ["null", "true", "false", "0", "-12", "3.5", "\"hi\""] {
            let v = parse(text).unwrap();
            assert_eq!(v.to_string(), text, "round trip {text}");
        }
    }

    #[test]
    fn parses_and_serializes_nested() {
        let v = parse(r#"{"a":[1,2,{"b":"x"}],"c":null}"#).unwrap();
        assert_eq!(v.get("a").unwrap().as_array().unwrap().len(), 3);
        assert_eq!(v.get("a").unwrap().as_array().unwrap()[2].get("b").unwrap().as_str(), Some("x"));
        // BTreeMap key order is deterministic.
        assert_eq!(v.to_string(), r#"{"a":[1,2,{"b":"x"}],"c":null}"#);
    }

    #[test]
    fn escapes_on_the_way_out_and_in() {
        let s = "line1\nline2\t\"quoted\"\\back";
        let j = Json::Str(s.to_string());
        let text = j.to_string();
        assert_eq!(text, r#""line1\nline2\t\"quoted\"\\back""#);
        assert_eq!(parse(&text).unwrap(), j);
    }

    #[test]
    fn decodes_unicode_escapes_and_surrogate_pairs() {
        assert_eq!(parse(r#""A""#).unwrap().as_str(), Some("A"));
        // U+1F600 GRINNING FACE as a surrogate pair.
        assert_eq!(parse(r#""😀""#).unwrap().as_str(), Some("😀"));
    }

    #[test]
    fn accepts_addresses_as_decimal_or_hex_strings() {
        assert_eq!(Json::Str("0x1000".into()).as_u64(), Some(0x1000));
        assert_eq!(Json::Str("4096".into()).as_u64(), Some(4096));
        // A big guest address that exceeds an f64's exact-integer range survives as a string.
        assert_eq!(Json::Str("0xfffffff000".into()).as_u64(), Some(0xffff_fff000));
    }

    #[test]
    fn rejects_malformed_input_without_panicking() {
        for bad in ["", "{", "[1,]", "{\"a\":}", "tru", "\"unterminated", "1.2.3", "{\"a\":1} x"] {
            assert!(parse(bad).is_err(), "must reject {bad:?}");
        }
    }

    #[test]
    fn integer_valued_floats_print_without_a_point() {
        assert_eq!(Json::Num(42.0).to_string(), "42");
        assert_eq!(Json::Num(-7.0).to_string(), "-7");
        assert_eq!(Json::Num(3.5).to_string(), "3.5");
    }
}
