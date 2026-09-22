//! An opt-in, bounded record of the first bytes each socket carried.
//!
//! # This is guest traffic, and it is off unless something turns it on
//!
//! **Everything this module can capture belongs to the guest, and some of it is secret.** The
//! bytes a socket carries in this runtime are `libroblox.so`'s: its HTTP requests, its cookies,
//! its `.ROBLOSECURITY` session token, whatever it decides to send. Nothing here is recorded
//! unless a caller has said [`record_first`] with a byte budget, nothing is printed on its own,
//! and **nothing is ever written to a file** — [`take`] hands the bytes to the caller that asked
//! for them and clears them out of this process. A diagnostic that defaults to on, or that leaves
//! a transcript on disk, is a credential leak with a helpful name.
//!
//! The switch is a function rather than an environment variable for the same reason
//! `Bionic::set_filesystem_root` is: this layer should not decide policy for its embedder. A test
//! that wants the record reads its own switch and calls [`record_first`]; nothing in
//! `omni-platform` reads the environment.
//!
//! # What it can and cannot show you, which is the whole point of reading this first
//!
//! It was asked for to answer one question — *what exactly does the engine put on the wire when it
//! fetches its client settings?* — and the honest answer it produced is **you cannot see that
//! here**, because the engine carries its own OpenSSL and this seam sits underneath it. What
//! [`Transcript::sent`] holds for the settings socket is a TLS `ClientHello` and then opaque
//! application-data records; the request line and the headers were encrypted a layer above.
//!
//! That is not a failure of the record, and it is worth having said out loud once rather than
//! rediscovered: a reader who expects `GET /v2/settings/... HTTP/1.1` here and finds `16 03 01`
//! will otherwise conclude something is wrong with the socket. [`Transcript::outline`] therefore
//! does the small amount of parsing that makes the bytes legible as what they are — the TLS record
//! framing, and the server name out of the `ClientHello`, which **is** real evidence about where
//! the request went and is the part that is sent in the clear.
//!
//! To read the plaintext you would have to be inside the guest's TLS, which means either finding
//! `SSL_write` in `libroblox.so` (it exports no OpenSSL symbol — MEASURED: 0 of 1,108 dynamic
//! symbols begin `SSL_`, `BIO_` or contain `OPENSSL`) or standing a certificate authority of this
//! project's own in front of the endpoint, which would mean changing the CA bundle the APK ships.
//! Neither is this module.
//!
//! # Bounded, per socket and per direction
//!
//! [`record_first`] takes the budget in bytes and it is enforced per socket per direction, so a
//! recording left on during a long session costs at most `2 × limit` per socket plus the counters.
//! The counters are **not** bounded and are the thing worth reading when the bytes stop: a
//! transcript that says 517 bytes captured and 41,236 sent is telling you the conversation
//! continued long after the record stopped looking.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;

/// The per-socket, per-direction byte budget, or zero when nothing is recording.
///
/// Read on every [`note`] call, which is every `send` and every `recv` this seam performs, so it
/// is a plain relaxed load and not a lock. When it is zero — the default, and the state every
/// process starts in — `note` returns before it touches anything else.
static LIMIT: AtomicUsize = AtomicUsize::new(0);

/// The next socket identity. Handed out whether or not anything is recording, because a socket
/// created before the switch was thrown still needs a stable name if the switch is thrown later.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Which way bytes went, from this process's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Direction {
    /// Out of this machine: what the guest wrote.
    Sent,
    /// Into this machine: what the peer answered.
    Received,
}

/// What one socket carried, as far as the budget allowed.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    /// The socket's identity, from [`next_id`]. Monotonic within a process and nothing else.
    pub socket: u64,
    /// Who it was connected to, if it ever connected, as the address prints.
    pub peer: Option<String>,
    /// The first bytes the guest wrote, up to the budget.
    pub sent: Vec<u8>,
    /// The first bytes that arrived, up to the budget.
    pub received: Vec<u8>,
    /// How many bytes were written in total — **not** bounded by the budget.
    pub sent_total: usize,
    /// How many bytes arrived in total — **not** bounded by the budget.
    pub received_total: usize,
}

impl Transcript {
    /// A human-readable summary: the peer, the counts, and what the captured bytes appear to be.
    ///
    /// **It reads the bytes rather than assuming them.** If the first byte is a plausible TLS
    /// record type it walks the record framing and, for a `ClientHello`, pulls out the server name
    /// extension; otherwise it prints the leading bytes as text where they are printable. Either
    /// way the answer is derived from what was captured, so a socket that really did carry plain
    /// HTTP would say so.
    #[must_use]
    pub fn outline(&self) -> String {
        let peer = self.peer.clone().unwrap_or_else(|| "<never connected>".to_string());
        let mut out = format!(
            "socket {} to {}: sent {} bytes ({} captured), received {} bytes ({} captured)",
            self.socket,
            peer,
            self.sent_total,
            self.sent.len(),
            self.received_total,
            self.received.len()
        );
        for (what, bytes) in [("sent", &self.sent), ("received", &self.received)] {
            if bytes.is_empty() {
                continue;
            }
            out.push_str(&format!("\n  {what}: {}", describe(bytes)));
        }
        out
    }
}

/// Say what a captured prefix looks like, without claiming more than the bytes support.
fn describe(bytes: &[u8]) -> String {
    if let Some(summary) = tls_outline(bytes) {
        return summary;
    }
    // Not TLS-shaped: show it as text as far as it is printable, which is what an unencrypted
    // protocol would give. Newlines are escaped so one transcript stays one line per direction.
    let printable: String = bytes
        .iter()
        .take_while(|byte| matches!(byte, 0x09 | 0x0a | 0x0d | 0x20..=0x7e))
        .map(|byte| match byte {
            0x0a => "\\n".to_string(),
            0x0d => "\\r".to_string(),
            other => char::from(*other).to_string(),
        })
        .collect();
    if printable.len() >= 8 {
        format!("text, {} bytes readable: {printable}", printable.len())
    } else {
        let head: Vec<String> = bytes.iter().take(16).map(|byte| format!("{byte:02x}")).collect();
        format!("opaque, first bytes {}", head.join(" "))
    }
}

/// Walk TLS record framing, or answer `None` when the bytes are not shaped like it.
///
/// Deliberately conservative: it requires the first record to carry a TLS 1.x version and to be
/// within the 16 KiB + 2 KiB a record may be, so ordinary binary data does not get announced as
/// TLS. A truncated final record is expected and is reported as such — the budget cuts the capture
/// mid-record almost every time.
fn tls_outline(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 5 {
        return None;
    }
    let kind = bytes[0];
    if !matches!(kind, 20..=23) || bytes[1] != 0x03 || bytes[2] > 0x04 {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut at = 0usize;
    let mut server_name = None;
    while at + 5 <= bytes.len() {
        let kind = bytes[at];
        let length = usize::from(u16::from_be_bytes([bytes[at + 3], bytes[at + 4]]));
        if !matches!(kind, 20..=23) || bytes[at + 1] != 0x03 || length > 18_432 {
            break;
        }
        let name = match kind {
            20 => "change-cipher-spec",
            21 => "alert",
            22 => "handshake",
            23 => "application-data",
            _ => "unknown",
        };
        let body = &bytes[at + 5..bytes.len().min(at + 5 + length)];
        if kind == 22 && server_name.is_none() {
            server_name = client_hello_server_name(body);
        }
        let complete = body.len() == length;
        parts.push(format!("{name} {length}{}", if complete { "" } else { " (truncated)" }));
        if !complete {
            break;
        }
        at += 5 + length;
    }
    if parts.is_empty() {
        return None;
    }
    let mut summary = format!("TLS records [{}]", parts.join(", "));
    if let Some(name) = server_name {
        summary.push_str(&format!("; ClientHello server_name = {name}"));
    }
    summary.push_str("; the HTTP request is inside this, encrypted, and is not visible here");
    Some(summary)
}

/// The `server_name` extension of a `ClientHello`, when the handshake body is one.
///
/// Byte-counted rather than parsed into a structure, because every length here is attacker-shaped
/// data and the only thing wanted out of it is one string. Every step bounds-checks and answers
/// `None` rather than panicking, which is what makes it safe to run over a guest's bytes.
fn client_hello_server_name(body: &[u8]) -> Option<String> {
    // Handshake header: type(1) length(3). Type 1 is ClientHello.
    if body.len() < 4 || body[0] != 1 {
        return None;
    }
    let mut at = 4usize;
    at += 2; // legacy_version
    at += 32; // random
    let session = *body.get(at)? as usize;
    at += 1 + session;
    let suites = usize::from(u16::from_be_bytes([*body.get(at)?, *body.get(at + 1)?]));
    at += 2 + suites;
    let compression = *body.get(at)? as usize;
    at += 1 + compression;
    let extensions_len = usize::from(u16::from_be_bytes([*body.get(at)?, *body.get(at + 1)?]));
    at += 2;
    let end = at.checked_add(extensions_len)?.min(body.len());
    while at + 4 <= end {
        let kind = u16::from_be_bytes([body[at], body[at + 1]]);
        let length = usize::from(u16::from_be_bytes([body[at + 2], body[at + 3]]));
        let data = body.get(at + 4..(at + 4 + length).min(body.len()))?;
        if kind == 0 {
            // server_name: list length(2), then name_type(1) + length(2) + host.
            let host = data.get(5..)?;
            let name = String::from_utf8_lossy(host).to_string();
            return Some(name);
        }
        at += 4 + length;
    }
    None
}

/// Every socket's transcript, keyed by identity so the order is the order sockets were made.
fn table() -> &'static Mutex<BTreeMap<u64, Transcript>> {
    static TABLE: OnceLock<Mutex<BTreeMap<u64, Transcript>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// A fresh socket identity.
pub(super) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Start recording, with a per-socket per-direction budget in bytes.
///
/// **Read this module's header before calling it.** The bytes are the guest's and may carry
/// credentials. A budget of zero is the same as [`stop`].
pub fn record_first(limit: usize) {
    LIMIT.store(limit, Ordering::Relaxed);
}

/// Stop recording. Anything already captured stays until [`take`].
pub fn stop() {
    LIMIT.store(0, Ordering::Relaxed);
}

/// Whether anything is being recorded.
#[must_use]
pub fn recording() -> bool {
    LIMIT.load(Ordering::Relaxed) != 0
}

/// Every transcript captured so far, **removed** from this process as they are handed over.
///
/// Taking rather than copying is deliberate: the caller that asked for the record is the only one
/// that should hold it, and a second caller getting the same credentials out of a global is the
/// shape this module exists to avoid.
#[must_use]
pub fn take() -> Vec<Transcript> {
    let mut table = table().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut *table).into_values().collect()
}

/// Record what a socket just carried. A no-op, and nearly free, when nothing is recording.
pub(super) fn note(socket: u64, direction: Direction, bytes: &[u8]) {
    let limit = LIMIT.load(Ordering::Relaxed);
    if limit == 0 || bytes.is_empty() {
        return;
    }
    let mut table = table().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = table.entry(socket).or_default();
    entry.socket = socket;
    let (buffer, total) = match direction {
        Direction::Sent => (&mut entry.sent, &mut entry.sent_total),
        Direction::Received => (&mut entry.received, &mut entry.received_total),
    };
    *total += bytes.len();
    let room = limit.saturating_sub(buffer.len());
    if room > 0 {
        buffer.extend_from_slice(&bytes[..room.min(bytes.len())]);
    }
}

/// Record who a socket connected to. A no-op when nothing is recording.
pub(super) fn note_peer(socket: u64, peer: &str) {
    if LIMIT.load(Ordering::Relaxed) == 0 {
        return;
    }
    let mut table = table().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = table.entry(socket).or_default();
    entry.socket = socket;
    entry.peer = Some(peer.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The default is off, and `note` must not accumulate anything while it is.**
    ///
    /// The failure this catches is the one the module header is about: a recorder that captures
    /// guest traffic without being asked is a credential leak, and it would look exactly like a
    /// working recorder from every other test.
    #[test]
    fn nothing_is_captured_until_something_asks() {
        let _guard = serial();
        stop();
        let _ = take();
        let id = next_id();
        note(id, Direction::Sent, b"GET /secret HTTP/1.1\r\n");
        assert!(!recording());
        assert!(take().is_empty(), "a recorder that is off must capture nothing");
    }

    /// The budget bounds the bytes and **not** the counters, which is what makes a truncated
    /// capture readable as truncated rather than as a short conversation.
    #[test]
    fn the_budget_bounds_the_bytes_and_not_the_count() {
        let _guard = serial();
        let _ = take();
        record_first(8);
        let id = next_id();
        note(id, Direction::Sent, b"0123456789");
        note(id, Direction::Sent, b"abcdefghij");
        note(id, Direction::Received, b"xy");
        stop();
        let transcripts = take();
        let one = transcripts.iter().find(|t| t.socket == id).expect("the socket was recorded");
        assert_eq!(one.sent, b"01234567", "the budget is per direction and is a prefix");
        assert_eq!(one.sent_total, 20, "the count is what really went, not what was kept");
        assert_eq!(one.received, b"xy");
        assert_eq!(one.received_total, 2);
    }

    /// Taking clears, so a second caller cannot read the first caller's credentials back out.
    #[test]
    fn taking_the_record_removes_it() {
        let _guard = serial();
        let _ = take();
        record_first(4);
        let id = next_id();
        note(id, Direction::Sent, b"abcd");
        stop();
        assert!(take().iter().any(|t| t.socket == id));
        assert!(take().iter().all(|t| t.socket != id), "take must not leave a copy behind");
    }

    /// A real `ClientHello` prefix is reported as TLS, with the server name that is genuinely in
    /// the clear — and the outline says the HTTP is *not* visible, because that is the finding
    /// this whole module exists to make legible.
    #[test]
    fn a_client_hello_is_read_as_tls_and_gives_up_its_server_name() {
        let host = b"clientsettingscdn.roblox.com";
        let mut extension = vec![0x00, 0x00]; // server_name
        let mut name = vec![0x00, 0x00, 0x00]; // list length placeholder + name_type
        name.extend_from_slice(&(host.len() as u16).to_be_bytes());
        name.extend_from_slice(host);
        let list_len = (name.len() - 2) as u16;
        name[0..2].copy_from_slice(&list_len.to_be_bytes());
        extension.extend_from_slice(&(name.len() as u16).to_be_bytes());
        extension.extend_from_slice(&name);

        let mut hello = vec![0x03, 0x03];
        hello.extend_from_slice(&[0u8; 32]);
        hello.push(0); // session id length
        hello.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher suites
        hello.extend_from_slice(&[0x01, 0x00]); // compression
        hello.extend_from_slice(&(extension.len() as u16).to_be_bytes());
        hello.extend_from_slice(&extension);

        let mut handshake = vec![1u8];
        handshake.extend_from_slice(&(hello.len() as u32).to_be_bytes()[1..]);
        handshake.extend_from_slice(&hello);

        let mut record = vec![0x16, 0x03, 0x01];
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);

        let outline = describe(&record);
        assert!(outline.contains("TLS records"), "{outline}");
        // The **exact** name, with the delimiter after it: a hostname reported with a stray
        // length byte in front of it still "contains" the hostname, and a diagnostic that names
        // the wrong host is worse than one that names none.
        assert!(outline.contains("server_name = clientsettingscdn.roblox.com;"), "{outline}");
        assert!(outline.contains("not visible here"), "{outline}");
    }

    /// Plain text is **not** announced as TLS. Without this, a socket that really did carry an
    /// unencrypted request would be summarised as encrypted and the diagnostic would be lying in
    /// exactly the direction that matters.
    #[test]
    fn plain_http_is_reported_as_text() {
        let outline = describe(b"GET /v2/settings/application/GoogleAndroidApp HTTP/1.1\r\nHost: x");
        assert!(outline.starts_with("text,"), "{outline}");
        assert!(outline.contains("GET /v2/settings"), "{outline}");
    }

    /// A malformed `ClientHello` must answer `None` rather than panic: every length in it is
    /// guest data, and this parser runs over whatever the guest sent.
    #[test]
    fn a_truncated_client_hello_does_not_panic() {
        for cut in 0..80usize {
            let mut record = vec![0x16, 0x03, 0x01, 0x00, 0xff];
            record.extend(std::iter::repeat_n(0xAAu8, cut));
            let _ = describe(&record);
        }
    }

    /// These tests share one global switch, so they take one lock rather than racing each other.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
