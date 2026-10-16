//! Abstract unix sockets across the host processes of an instance. A socket name is a kernel's,
//! the whole device's; here each host process keeps its own (`crate::unix`), so a name a process
//! of one host process listens on was ENOENT to every other. The WebView zygote -- started as an
//! app is, in a host process of its own -- listens on `@com.android.internal.os.WebViewZygoteInit/
//! <uuid>`, and ActivityManager, in the system's, retried its connect 2,402 times with its locks
//! held: system_server stalled, an app's binder call to it waited 50 s, and the app was killed for
//! not answering input (r10).
//!
//! A listening abstract socket is published in the instance (`<instance>/.omni-sockets/<hex of
//! the name>`, holding this host process's broker port). A connect that finds no local socket of
//! that name asks the owner's broker: the client's host process makes a socket pair, offers its far
//! end on a relay port (`crate::relay`), and the broker connects a client of its own to the
//! listening socket -- whose `accept` then sees it, with the caller's credentials -- and attaches
//! that client to the relay. What either side writes crosses as the relay's framed messages.
//!
//! Only abstract names cross: `/dev/socket/<name>` stays the host process's own, so an app's DNS is
//! still the kernel's (`crate::dnsproxy`) and its `fwmarkd` connect still fails as libnetd_client
//! expects (no descriptors pass through the relay).
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::errno::{Errno, ECONNREFUSED};
use crate::fd::{FileKind, OpenFile};
use crate::socket::{Cred, Socket};

fn entry(instance: &Path, name: &[u8]) -> PathBuf {
    let hex: String = name.iter().map(|b| format!("{b:02x}")).collect();
    instance.join(".omni-sockets").join(hex)
}

/// This host process's broker: the port other host processes ask to connect to a name.
fn broker() -> Option<u16> {
    static PORT: OnceLock<Option<u16>> = OnceLock::new();
    *PORT.get_or_init(|| {
        let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
        let port = listener.local_addr().ok()?.port();
        std::thread::Builder::new()
            .name("xsocket-broker".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    std::thread::spawn(move || serve(stream));
                }
            })
            .ok()?;
        Some(port)
    })
}

/// One request: `[name len u32][name][relay port u16][ty u64][pid, uid, gid u32]`; the answer is
/// `[errno i32 (0: connected)][server pid, uid, gid u32]`.
fn serve(mut stream: TcpStream) {
    let mut len = [0u8; 4];
    if stream.read_exact(&mut len).is_err() {
        return;
    }
    let mut name = vec![0u8; u32::from_le_bytes(len) as usize];
    let mut rest = [0u8; 2 + 8 + 12];
    if stream.read_exact(&mut name).is_err() || stream.read_exact(&mut rest).is_err() {
        return;
    }
    let port = u16::from_le_bytes([rest[0], rest[1]]);
    let ty = u64::from_le_bytes(rest[2..10].try_into().expect("8"));
    let word = |i: usize| u32::from_le_bytes(rest[10 + 4 * i..14 + 4 * i].try_into().expect("4"));
    let cred: Cred = [word(0), word(1), word(2)];
    let (errno, server) = match crate::unix::Bound::find_any(&name) {
        None => (ECONNREFUSED.0, [0; 3]),
        Some(bound) => {
            let mut client = Socket { domain: 1, ty, peer: None, inbox: std::collections::VecDeque::new(), name: None, protocol: 0, owner: cred[0], passcred: false };
            match bound.connect(&mut client, cred) {
                Ok(()) => {
                    let file = Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::Socket(client)), flags: parking_lot::Mutex::new(2) });
                    match crate::relay::attach(file, port) {
                        Ok(()) => (0, bound.cred),
                        Err(_) => (ECONNREFUSED.0, [0; 3]),
                    }
                }
                Err(e) => (e.0, [0; 3]),
            }
        }
    };
    let mut answer = errno.to_le_bytes().to_vec();
    for w in server {
        answer.extend_from_slice(&w.to_le_bytes());
    }
    let _ = stream.write_all(&answer);
}

/// A socket bound to abstract `name` in this host process listens: other host processes of the
/// instance may connect to it from now on.
pub fn publish(instance: &Path, name: &[u8]) {
    if name.first() != Some(&b'@') {
        return;
    }
    let Some(port) = broker() else { return };
    let path = entry(instance, name);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, port.to_string());
}

/// Connect `client` to abstract `name` listened on in another host process of the instance: `None`
/// when no host process published it.
pub fn connect(instance: &Path, name: &[u8], client: &mut Socket, cred: Cred) -> Option<Result<(), Errno>> {
    if name.first() != Some(&b'@') || client.ty == 2 {
        return None;
    }
    let port: u16 = std::fs::read_to_string(entry(instance, name)).ok()?.trim().parse().ok()?;
    Some((|| {
        let (mine, other) = crate::socket::pair(client.ty, cred, [0; 3]);
        let held = Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::Socket(other)), flags: parking_lot::Mutex::new(2) });
        let relay = crate::relay::offer(held).ok_or(ECONNREFUSED)?;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|_| ECONNREFUSED)?;
        let mut request = (name.len() as u32).to_le_bytes().to_vec();
        request.extend_from_slice(name);
        request.extend_from_slice(&relay.to_le_bytes());
        request.extend_from_slice(&client.ty.to_le_bytes());
        for w in cred {
            request.extend_from_slice(&w.to_le_bytes());
        }
        stream.write_all(&request).map_err(|_| ECONNREFUSED)?;
        let mut answer = [0u8; 16];
        stream.read_exact(&mut answer).map_err(|_| ECONNREFUSED)?;
        let errno = i32::from_le_bytes(answer[0..4].try_into().expect("4"));
        if errno != 0 {
            return Err(Errno(errno));
        }
        client.peer = Some(mine);
        Ok(())
    })())
}

/// The abstract names an instance's host processes listen on now (each with its `@`), as
/// published in `instance`: some may belong to processes that have since ended.
#[must_use]
pub fn published(instance: &Path) -> Vec<Vec<u8>> {
    let Ok(entries) = std::fs::read_dir(instance.join(".omni-sockets")) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let hex = e.file_name().to_string_lossy().into_owned();
            (hex.len() % 2 == 0).then_some(())?;
            (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
        })
        .collect()
}

/// **A host program's connection to an abstract socket a guest listens on** -- the MCP server's to
/// a WebView's DevTools (`@webview_devtools_remote_<pid>`). It asks the listening host process's
/// broker as another host process does, and stands in for that host process's side of the relay:
/// what it writes reaches the guest's accepted socket, what the guest writes is read here.
/// Descriptors the guest sends with its bytes are not taken.
pub struct HostStream {
    stream: TcpStream,
    /// What is left of the frame being read.
    left: usize,
    /// Descriptor descriptions follow the frame's bytes.
    rights: bool,
}

impl HostStream {
    /// Connect to abstract `name` (with its `@`) in `instance` as `cred` (`pid, uid, gid`).
    ///
    /// # Errors
    /// Nothing listens on it (any more), or the broker could not be reached.
    pub fn connect(instance: &Path, name: &[u8], cred: Cred) -> std::io::Result<Self> {
        use std::io::{Error, ErrorKind};
        let port: u16 = std::fs::read_to_string(entry(instance, name))?.trim().parse().map_err(|_| Error::new(ErrorKind::InvalidData, "a broker port"))?;
        let relay = TcpListener::bind(("127.0.0.1", 0))?;
        let mut broker = TcpStream::connect(("127.0.0.1", port))?;
        broker.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        let mut request = (name.len() as u32).to_le_bytes().to_vec();
        request.extend_from_slice(name);
        request.extend_from_slice(&relay.local_addr()?.port().to_le_bytes());
        request.extend_from_slice(&1u64.to_le_bytes()); // SOCK_STREAM
        for w in cred {
            request.extend_from_slice(&w.to_le_bytes());
        }
        broker.write_all(&request)?;
        let mut answer = [0u8; 16];
        broker.read_exact(&mut answer)?;
        let errno = i32::from_le_bytes(answer[0..4].try_into().expect("4"));
        if errno != 0 {
            return Err(Error::new(ErrorKind::ConnectionRefused, format!("{} refused (errno {errno})", String::from_utf8_lossy(name))));
        }
        // The broker attached its end of the relay before it answered: waiting in the backlog.
        relay.set_nonblocking(true)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let stream = loop {
            match relay.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == ErrorKind::WouldBlock && std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(2)),
                Err(e) => return Err(e),
            }
        };
        stream.set_nonblocking(false)?;
        stream.set_nodelay(true)?;
        Ok(Self { stream, left: 0, rights: false })
    }

    /// How long a read may wait (`None`: for ever).
    ///
    /// # Errors
    /// As `TcpStream::set_read_timeout`.
    pub fn set_read_timeout(&self, limit: Option<std::time::Duration>) -> std::io::Result<()> {
        self.stream.set_read_timeout(limit)
    }

    fn u32(&mut self) -> std::io::Result<u32> {
        let mut b = [0u8; 4];
        self.stream.read_exact(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }
}

impl Read for HostStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.left == 0 {
            if self.rights {
                // Descriptors sent with the last frame: skipped (their relays time out unattached).
                self.rights = false;
                for _ in 0..self.u32()? {
                    let n = self.u32()? as usize;
                    std::io::copy(&mut (&mut self.stream).take(n as u64), &mut std::io::sink())?;
                }
            }
            let mut len = [0u8; 4];
            match self.stream.read(&mut len[..1])? {
                0 => return Ok(0),
                _ => self.stream.read_exact(&mut len[1..])?,
            }
            let len = u32::from_le_bytes(len);
            self.left = (len & !(1 << 31)) as usize;
            self.rights = len & (1 << 31) != 0;
        }
        let n = buf.len().min(self.left);
        let n = self.stream.read(&mut buf[..n])?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n;
        Ok(n)
    }
}

impl Write for HostStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = buf.len().min(1 << 20);
        let mut frame = (n as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&buf[..n]);
        self.stream.write_all(&frame)?;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}
