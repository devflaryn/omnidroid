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
