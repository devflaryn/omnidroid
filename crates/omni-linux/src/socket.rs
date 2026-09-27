//! Sockets. One service stands behind an address here: `logd`'s write socket
//! (`/dev/socket/logdw`), whose packets are printed as `logcat` would print them, so what ART and
//! the framework log -- an abort's reason above all -- is seen. Every other address is refused at
//! `connect` as it is on a device where that service is not running, and the caller takes its
//! no-service path.
//!
//! `socketpair(AF_UNIX)` makes two connected ends ([`PairChannel`]): SurfaceFlinger's `BitTube`
//! (vsync events) and an app's `InputChannel` (input) -- ends that are passed to other processes
//! over binder, so the channel is shared, not owned by a process.
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

/// The two directions of a socket pair: what each end has been sent, and whether each is open.
pub struct PairChannel {
    /// `queues[side]`: what `side` has been sent (one entry a message; a stream's are merged).
    queues: Mutex<[std::collections::VecDeque<Vec<u8>>; 2]>,
    open: [std::sync::atomic::AtomicBool; 2],
    /// `SOCK_STREAM`: bytes; otherwise (`SOCK_SEQPACKET`, `SOCK_DGRAM`) whole messages.
    stream: bool,
}

/// What a connected socket talks to.
pub enum Peer {
    /// The other end of a socket pair.
    Pair { channel: Arc<PairChannel>, side: usize },
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

impl Drop for Socket {
    /// The last descriptor of an end is closed: the other end is hung up.
    fn drop(&mut self) {
        if let Some(Peer::Pair { channel, side }) = &self.peer {
            channel.open[*side].store(false, std::sync::atomic::Ordering::SeqCst);
            crate::poll::notify();
        }
    }
}

/// Deliver `bytes` to a connected socket's peer.
pub fn send(socket: &mut Socket, bytes: &[u8]) -> Result<usize, Errno> {
    let Socket { peer, inbox, .. } = socket;
    match peer {
        Some(Peer::Pair { channel, side }) => {
            let other = 1 - *side;
            if !channel.open[other].load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::errno::EPIPE);
            }
            channel.queues.lock()[other].push_back(bytes.to_vec());
            crate::poll::notify();
            Ok(bytes.len())
        }
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

/// Take what the peer sent back: all of it that fits, or `EAGAIN` when nothing is there. From a
/// socket pair: one message (the rest of it discarded, as a datagram's is) or a stream's bytes;
/// end of file (0) once the other end is closed and nothing is left.
pub fn receive(socket: &mut Socket, buf: &mut [u8]) -> Result<usize, Errno> {
    if let Some(Peer::Pair { channel, side }) = &socket.peer {
        let mut queues = channel.queues.lock();
        let q = &mut queues[*side];
        if q.is_empty() {
            return if channel.open[1 - *side].load(std::sync::atomic::Ordering::SeqCst) { Err(crate::errno::EAGAIN) } else { Ok(0) };
        }
        if !channel.stream {
            let msg = q.pop_front().expect("not empty");
            let n = msg.len().min(buf.len());
            buf[..n].copy_from_slice(&msg[..n]);
            return Ok(n);
        }
        let mut n = 0;
        while n < buf.len() {
            let Some(front) = q.front_mut() else { break };
            let take = front.len().min(buf.len() - n);
            buf[n..n + take].copy_from_slice(&front[..take]);
            front.drain(..take);
            if front.is_empty() {
                q.pop_front();
            }
            n += take;
        }
        return Ok(n);
    }
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

/// `MSG_DONTWAIT`.
const MSG_DONTWAIT: u64 = 0x40;

fn sys_recvfrom(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let mut buf = vec![0u8; (a[2] as usize).min(1 << 20)];
    let n = receive_waiting(&file, &mut buf, a[3] & MSG_DONTWAIT != 0, t)?;
    p.mem.write(a[1], &buf[..n])?;
    Ok(n as u64)
}

/// Receive from `file`, waiting (unless it or the call is non-blocking) while a socket pair's end
/// has nothing to read and its other end is open.
fn receive_waiting(file: &OpenFile, buf: &mut [u8], dontwait: bool, t: &Task) -> Result<usize, Errno> {
    loop {
        let seen = crate::poll::generation();
        let (r, pair) = {
            let mut kind = file.kind.lock();
            let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
            (receive(socket, buf), matches!(socket.peer, Some(Peer::Pair { .. })))
        };
        let nonblocking = dontwait || *file.flags.lock() & 0o4000 != 0;
        match r {
            Err(e) if e == crate::errno::EAGAIN && pair && !nonblocking => crate::poll::wait_for_change(seen, None, t)?,
            other => return other,
        }
    }
}

/// `read` of a socket pair's end (which may wait); `None` for anything else.
pub fn read(file: &OpenFile, buf: &mut [u8], t: &Task) -> Option<Result<usize, Errno>> {
    let pair = matches!(&*file.kind.lock(), FileKind::Socket(Socket { peer: Some(Peer::Pair { .. }), .. }));
    pair.then(|| receive_waiting(file, buf, false, t))
}

/// What a socket is ready for: a pair's end is readable with something queued, writable while the
/// other end is open, and hung up (readable, `POLLHUP`) when it is closed.
#[must_use]
pub fn readiness(socket: &Socket) -> u32 {
    const IN: u32 = 0x1;
    const OUT: u32 = 0x4;
    const HUP: u32 = 0x10;
    match &socket.peer {
        Some(Peer::Pair { channel, side }) => {
            let queued = !channel.queues.lock()[*side].is_empty();
            let other_open = channel.open[1 - *side].load(std::sync::atomic::Ordering::SeqCst);
            (if queued { IN } else { 0 }) | if other_open { OUT } else { IN | HUP }
        }
        _ => (if socket.inbox.is_empty() { 0 } else { IN }) | OUT,
    }
}

/// `socketpair(AF_UNIX, type, 0, sv)`.
fn sys_socketpair(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let (domain, ty) = (a[0], a[1]);
    if domain != AF_UNIX {
        return Err(EAFNOSUPPORT);
    }
    const SOCK_DGRAM: u64 = 2;
    const SOCK_SEQPACKET: u64 = 5;
    let kind = ty & SOCK_TYPE_MASK;
    if !matches!(kind, 1 | SOCK_DGRAM | SOCK_SEQPACKET) {
        return Err(EINVAL);
    }
    let channel = Arc::new(PairChannel {
        queues: Mutex::new([std::collections::VecDeque::new(), std::collections::VecDeque::new()]),
        open: [std::sync::atomic::AtomicBool::new(true), std::sync::atomic::AtomicBool::new(true)],
        stream: kind == 1,
    });
    let flags = if ty & SOCK_NONBLOCK != 0 { 0o4000 } else { 0 } | 2; // O_RDWR
    let mut fds = [0i32; 2];
    for (side, fd) in fds.iter_mut().enumerate() {
        let socket = Socket { domain, ty: kind, peer: Some(Peer::Pair { channel: Arc::clone(&channel), side }), inbox: std::collections::VecDeque::new() };
        let file = OpenFile { kind: Mutex::new(FileKind::Socket(socket)), flags: Mutex::new(flags) };
        *fd = p.fds.insert(Arc::new(file), ty & SOCK_CLOEXEC != 0, 0)?;
    }
    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&fds[0].to_le_bytes());
    out[4..8].copy_from_slice(&fds[1].to_le_bytes());
    p.mem.write(a[3], &out)?;
    Ok(0)
}

/// `recvmsg`: into the message's iovecs, no control data (nothing here passes descriptors over a
/// socket).
fn sys_recvmsg(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let (iov, iovlen) = (p.mem.read_u64(a[1] + 16)?, p.mem.read_u64(a[1] + 24)?);
    if iovlen > 1024 {
        return Err(EINVAL);
    }
    let mut spans = Vec::new();
    let mut total = 0usize;
    for i in 0..iovlen {
        let (base, len) = (p.mem.read_u64(iov + i * 16)?, p.mem.read_u64(iov + i * 16 + 8)? as usize);
        spans.push((base, len));
        total = total.saturating_add(len);
    }
    let mut buf = vec![0u8; total.min(1 << 20)];
    let n = receive_waiting(&file, &mut buf, a[2] & MSG_DONTWAIT != 0, t)?;
    let mut at = 0;
    for (base, len) in spans {
        if at >= n {
            break;
        }
        let k = len.min(n - at);
        p.mem.write(base, &buf[at..at + k])?;
        at += k;
    }
    // No control data; msg_flags 0.
    p.mem.write(a[1] + 40, &0u64.to_le_bytes())?;
    p.mem.write(a[1] + 48, &0u32.to_le_bytes())?;
    Ok(n as u64)
}

pub fn install(table: &mut Table) {
    table.set(nr::RECVFROM, sys_recvfrom);
    table.set(nr::RECVMSG, sys_recvmsg);
    table.set(nr::SOCKETPAIR, sys_socketpair);
    table.set(nr::SOCKET, sys_socket);
    table.set(nr::GETSOCKOPT, sys_getsockopt);
    table.set(nr::CONNECT, sys_connect);
    table.set(nr::SENDTO, sys_sendto);
    table.set(nr::SENDMSG, sys_sendmsg);
    table.set(nr::SETSOCKOPT, sys_setsockopt);
    table.set(nr::SHUTDOWN, sys_shutdown);
}
