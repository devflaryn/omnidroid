//! Sockets. One service stands behind an address here: `logd`'s write socket
//! (`/dev/socket/logdw`), whose packets are printed as `logcat` would print them, so what ART and
//! the framework log -- an abort's reason above all -- is seen. Every other address is refused at
//! `connect` as it is on a device where that service is not running, and the caller takes its
//! no-service path.
use std::sync::Arc;

use parking_lot::Mutex;

use crate::errno::{Errno, SysResult, EAFNOSUPPORT, EINVAL, ENOENT, ENOTCONN};
use crate::fd::{FileKind, OpenFile, Output};
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};

const AF_UNIX: u64 = 1;
const AF_INET: u64 = 2;
const AF_INET6: u64 = 10;
const AF_NETLINK: u64 = 16;
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;

/// What a connected socket talks to.
pub enum Peer {
    /// `logd`: packets are printed to this output.
    Logd(Output),
    /// init's property service: `setprop` requests, answered one `u32` each.
    PropertyService { pending: Vec<u8>, service: std::sync::Arc<crate::props::PropertyService> },
}

pub struct Socket {
    pub domain: u64,
    pub ty: u64,
    pub peer: Option<Peer>,
    /// What the peer sent back, not yet read.
    pub inbox: std::collections::VecDeque<u8>,
}

/// The property service's protocol 2: `PROP_MSG_SETPROP2`, then the name and the value, each
/// a `u32` length and its bytes. Complete requests are applied and answered.
fn property_requests(pending: &mut Vec<u8>, service: &crate::props::PropertyService, inbox: &mut std::collections::VecDeque<u8>) {
    const PROP_MSG_SETPROP2: u32 = 0x0002_0001;
    loop {
        let word = |b: &[u8], at: usize| b.get(at..at + 4).map(|w| u32::from_le_bytes(w.try_into().expect("4")) as usize);
        let Some(cmd) = word(pending, 0) else { return };
        let Some(name_len) = word(pending, 4) else { return };
        let Some(value_len) = word(pending, 8 + name_len) else { return };
        let total = 12 + name_len + value_len;
        if pending.len() < total {
            return;
        }
        let name = String::from_utf8_lossy(&pending[8..8 + name_len]).into_owned();
        let value = String::from_utf8_lossy(&pending[12 + name_len..total]).into_owned();
        pending.drain(..total);
        let result = if cmd as u32 == PROP_MSG_SETPROP2 { service.set(&name, &value) } else { 0xfe };
        inbox.extend(result.to_le_bytes());
    }
}

/// One `logd` packet, as liblog's `logd_writer` sends it: a header `{ log_id u8, tid u16, sec
/// u32, nsec u32 }`, then (for the text logs) priority `u8`, a NUL-terminated tag and a
/// NUL-terminated message. The binary logs (events, stats, security) are not text; they are
/// accepted and not printed.
#[must_use]
pub fn format_log(packet: &[u8]) -> Option<String> {
    let (&id, rest) = packet.split_first()?;
    if !matches!(id, 0 | 1 | 3 | 4) || rest.len() < 10 {
        return None;
    }
    let tid = u16::from_le_bytes([rest[0], rest[1]]);
    let body = &rest[10..];
    let (&prio, body) = body.split_first()?;
    let mut parts = body.splitn(2, |&b| b == 0);
    let tag = String::from_utf8_lossy(parts.next()?).into_owned();
    let msg = parts.next().unwrap_or_default();
    let msg = String::from_utf8_lossy(msg.strip_suffix(&[0]).unwrap_or(msg)).into_owned();
    let level = match prio {
        2 => 'V',
        3 => 'D',
        4 => 'I',
        5 => 'W',
        6 => 'E',
        7 => 'F',
        _ => '?',
    };
    let mut out = String::new();
    for line in msg.lines() {
        out += &format!("{level}/{tag}({tid:5}): {line}\n");
    }
    if out.is_empty() {
        out = format!("{level}/{tag}({tid:5}): \n");
    }
    Some(out)
}

/// Deliver `bytes` to a connected socket's peer.
pub fn send(socket: &mut Socket, bytes: &[u8]) -> Result<usize, Errno> {
    let Socket { peer, inbox, .. } = socket;
    match peer {
        Some(Peer::PropertyService { pending, service }) => {
            pending.extend_from_slice(bytes);
            let service = std::sync::Arc::clone(service);
            property_requests(pending, &service, inbox);
            Ok(bytes.len())
        }
        Some(Peer::Logd(out)) => {
            if let Some(text) = format_log(bytes) {
                match out {
                    Output::Host => eprint!("{text}"),
                    Output::Capture(buf) => buf.lock().extend_from_slice(text.as_bytes()),
                }
            }
            Ok(bytes.len())
        }
        None => Err(ENOTCONN),
    }
}

fn sys_socket(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let (domain, ty) = (a[0], a[1]);
    if !matches!(domain, AF_UNIX | AF_INET | AF_INET6 | AF_NETLINK) {
        return Err(EAFNOSUPPORT);
    }
    let socket = Socket { domain, ty: ty & SOCK_TYPE_MASK, peer: None, inbox: std::collections::VecDeque::new() };
    let flags = if ty & SOCK_NONBLOCK != 0 { 0o4000 } else { 0 } | 2; // O_RDWR
    let file = OpenFile { kind: Mutex::new(FileKind::Socket(socket)), flags: Mutex::new(flags) };
    Ok(p.fds.insert(Arc::new(file), ty & SOCK_CLOEXEC != 0, 0)? as u64)
}

/// The path in a `sockaddr_un` (`sun_path`, NUL-terminated or filling the length); abstract
/// addresses (a leading NUL) come back with an `@`, as `ss` shows them.
fn unix_path(p: &Process, at: u64, len: u64) -> Result<Vec<u8>, Errno> {
    if !(2..=110).contains(&len) {
        return Err(EINVAL);
    }
    let raw = p.mem.read(at, len as usize)?;
    let path = &raw[2..];
    Ok(match path.first() {
        Some(0) => [b"@".as_slice(), &path[1..]].concat(),
        _ => path.split(|&b| b == 0).next().unwrap_or_default().to_vec(),
    })
}

fn sys_connect(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let stderr = match p.fds.get(2).ok().as_deref().map(|f| f.kind.lock().output()) {
        Some(Some(out)) => out,
        _ => Output::Host,
    };
    let mut kind = file.kind.lock();
    let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
    if socket.domain != AF_UNIX {
        // No network: an address that cannot be reached.
        return Err(crate::errno::ENETUNREACH);
    }
    let path = unix_path(p, a[1], a[2])?;
    if path == b"/dev/socket/logdw" {
        socket.peer = Some(Peer::Logd(stderr));
        return Ok(0);
    }
    if path == b"/dev/socket/property_service" {
        let service = crate::props::PropertyService::global(p.vfs.sysroot());
        socket.peer = Some(Peer::PropertyService { pending: Vec::new(), service });
        return Ok(0);
    }
    if p.trace {
        eprintln!("[socket] connect {:?}: no service", String::from_utf8_lossy(&path));
    }
    Err(ENOENT)
}

fn sys_sendto(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let bytes = p.mem.read(a[1], (a[2] as usize).min(1 << 20))?;
    let mut kind = file.kind.lock();
    let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
    Ok(send(socket, &bytes)? as u64)
}

fn sys_sendmsg(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    // struct msghdr: name, namelen, iov, iovlen, control, controllen, flags.
    let (iov, iovlen) = (p.mem.read_u64(a[1] + 16)?, p.mem.read_u64(a[1] + 24)?);
    if iovlen > 1024 {
        return Err(EINVAL);
    }
    let mut bytes = Vec::new();
    for i in 0..iovlen {
        let (base, len) = (p.mem.read_u64(iov + i * 16)?, p.mem.read_u64(iov + i * 16 + 8)? as usize);
        bytes.extend_from_slice(&p.mem.read(base, len.min((1 << 20) - bytes.len().min(1 << 20)))?);
    }
    let mut kind = file.kind.lock();
    let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
    Ok(send(socket, &bytes)? as u64)
}

/// Options are accepted and not acted on: timeouts, buffer sizes and credentials change nothing
/// for a socket with no peer or with `logd`.
fn sys_setsockopt(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let is_socket = matches!(&*file.kind.lock(), FileKind::Socket(_));
    if is_socket { Ok(0) } else { Err(crate::errno::ENOTSOCK) }
}

fn sys_shutdown(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    sys_setsockopt(p, _t, a)
}

/// Take what the peer sent back: all of it that fits, or `EAGAIN` when nothing is there.
pub fn receive(socket: &mut Socket, buf: &mut [u8]) -> Result<usize, Errno> {
    if socket.inbox.is_empty() {
        return Err(crate::errno::EAGAIN);
    }
    let n = buf.len().min(socket.inbox.len());
    for (slot, b) in buf.iter_mut().zip(socket.inbox.drain(..n)) {
        *slot = b;
    }
    Ok(n)
}

/// `getsockopt`: the few options callers read. `SO_PEERCRED` reports the peer as init (a local
/// service's client-credential check then passes); `SO_TYPE` the socket's type; `SO_ERROR` none;
/// buffer sizes a plausible value. Everything else is zeroed.
fn sys_getsockopt(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let ty = match &*file.kind.lock() {
        FileKind::Socket(s) => s.ty,
        _ => return Err(crate::errno::ENOTSOCK),
    };
    let (level, name, val, len_ptr) = (a[1], a[2], a[3], a[4]);
    let write = |bytes: &[u8]| -> SysResult {
        let cap = p.mem.read_u32(len_ptr)? as usize;
        let n = bytes.len().min(cap);
        p.mem.write(val, &bytes[..n])?;
        p.mem.write_u32(len_ptr, n as u32)?;
        Ok(0)
    };
    const SOL_SOCKET: u64 = 1;
    if level == SOL_SOCKET {
        match name {
            17 => {
                // SO_PEERCRED: struct ucred { pid, uid, gid }.
                let mut c = [0u8; 12];
                c[0..4].copy_from_slice(&1i32.to_le_bytes());
                return write(&c);
            }
            3 => return write(&(ty as u32).to_le_bytes()), // SO_TYPE
            4 => return write(&0u32.to_le_bytes()),        // SO_ERROR
            7 | 8 => return write(&(256 * 1024u32).to_le_bytes()), // SO_SNDBUF/RCVBUF
            _ => return write(&0u32.to_le_bytes()),
        }
    }
    write(&0u32.to_le_bytes())
}

fn sys_recvfrom(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let mut buf = vec![0u8; (a[2] as usize).min(1 << 20)];
    let n = {
        let mut kind = file.kind.lock();
        let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
        receive(socket, &mut buf)?
    };
    p.mem.write(a[1], &buf[..n])?;
    Ok(n as u64)
}

pub fn install(table: &mut Table) {
    table.set(nr::RECVFROM, sys_recvfrom);
    table.set(nr::SOCKET, sys_socket);
    table.set(nr::GETSOCKOPT, sys_getsockopt);
    table.set(nr::CONNECT, sys_connect);
    table.set(nr::SENDTO, sys_sendto);
    table.set(nr::SENDMSG, sys_sendmsg);
    table.set(nr::SETSOCKOPT, sys_setsockopt);
    table.set(nr::SHUTDOWN, sys_shutdown);
}
