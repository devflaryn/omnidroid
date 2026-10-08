//! Sockets. One service stands behind an address here: `logd`'s write socket
//! (`/dev/socket/logdw`), whose packets are printed as `logcat` would print them, so what ART and
//! the framework log -- an abort's reason above all -- is seen. A Unix address no socket is bound
//! to is refused at `connect` as it is on a device where that service is not running, and the
//! caller takes its no-service path. A TCP or UDP socket is a host socket on the host's network
//! (`crate::hostnet`).
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
const AF_KEY: u64 = 15;
const AF_VSOCK: u64 = 40;
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;

/// A process's credentials, as a socket's peer knows them: `struct ucred { pid, uid, gid }`.
pub type Cred = [u32; 3];

/// `p`'s credentials.
#[must_use]
pub fn cred_of(p: &Process) -> Cred {
    [p.sys.pid as u32, p.sys.uid(), p.sys.gid()]
}

/// The two directions of a socket pair: what each end has been sent, and whether each is open.
pub struct PairChannel {
    /// `creds[side]`: the credentials of the process that made that end.
    creds: [Cred; 2],
    /// `queues[side]`: what `side` has been sent (one entry a message; a stream's are merged).
    queues: Mutex<[std::collections::VecDeque<Vec<u8>>; 2]>,
    open: [std::sync::atomic::AtomicBool; 2],
    /// `SOCK_STREAM`: bytes; otherwise (`SOCK_SEQPACKET`, `SOCK_DGRAM`) whole messages.
    stream: bool,
    /// An input channel's messages in flight (`crate::input_channel`), if this pair is one.
    input: Mutex<crate::input_channel::Channel>,
}

/// In an init socket's type: `+passcred`.
pub const INIT_PASSCRED: u64 = 1 << 32;

/// A socket init makes for a service (`socket <name> <type>`): bound to `/dev/socket/<name>` in
/// `instance`, as an open file to hand over.
#[must_use]
pub fn init_socket(instance: usize, name: &str, ty: u64, cred: Cred) -> OpenFile {
    let path = format!("/dev/socket/{name}");
    let passcred = ty & INIT_PASSCRED != 0;
    let ty = ty & SOCK_TYPE_MASK;
    let bound = crate::unix::Bound::bind_replacing(instance, path.as_bytes(), ty, cred);
    bound.passcred.store(passcred, std::sync::atomic::Ordering::SeqCst);
    let mut addr = (AF_UNIX as u16).to_le_bytes().to_vec();
    addr.extend_from_slice(path.as_bytes());
    addr.push(0);
    let socket = Socket { domain: AF_UNIX, ty, peer: Some(Peer::Bound(bound)), inbox: std::collections::VecDeque::new(), name: Some(addr), protocol: 0, owner: cred[0], passcred };
    OpenFile { kind: Mutex::new(FileKind::Socket(socket)), flags: Mutex::new(2) }
}

/// A connected pair of type `ty`: the client end's peer, and the server's socket.
#[must_use]
pub fn pair(ty: u64, client: Cred, server: Cred) -> (Peer, Socket) {
    let channel = Arc::new(PairChannel {
        creds: [client, server],
        queues: Mutex::new([std::collections::VecDeque::new(), std::collections::VecDeque::new()]),
        open: [std::sync::atomic::AtomicBool::new(true), std::sync::atomic::AtomicBool::new(true)],
        stream: ty == 1,
        input: Mutex::default(),
    });
    let server = Socket { domain: AF_UNIX, ty, peer: Some(Peer::Pair { channel: Arc::clone(&channel), side: 1 }), inbox: std::collections::VecDeque::new(), name: None, protocol: 0, owner: server[0], passcred: false };
    (Peer::Pair { channel, side: 0 }, server)
}

/// What a connected socket talks to.
pub enum Peer {
    /// This socket is bound to a name (`crate::unix`): it listens, or receives datagrams.
    Bound(Arc<crate::unix::Bound>),
    /// A datagram socket connected to a bound one: what it sends goes there.
    Dgram(Arc<crate::unix::Bound>),
    /// An internet socket bound to a port (`crate::inet`): a raw or ICMP socket, which the host's
    /// network does not stand behind.
    Inet(Arc<crate::inet::Port>),
    /// A TCP or UDP socket: a host socket on the host's network (`crate::hostnet`).
    Host(Arc<crate::hostnet::Host>),
    /// netd's DNS proxy, answered by the kernel where nothing in this host process is bound to
    /// `/dev/socket/dnsproxyd` (`crate::dnsproxy`).
    Dns(Arc<crate::dnsproxy::Proxy>),
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
    /// The address it is bound to, as `getsockname` reports it.
    pub name: Option<Vec<u8>>,
    /// The protocol it was made with (a netlink family: `NETLINK_ROUTE`, ...).
    pub protocol: u64,
    /// The port a netlink socket is bound to when it sends unbound: its process's pid.
    pub owner: u32,
    /// `SO_PASSCRED`: each message received carries its sender's credentials.
    pub passcred: bool,
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
    // OMNI_LOG_TIME=1: each line starts with the time the entry was written (seconds, from the
    // entry's own header), as logcat's `-v time` shows it.
    static TIMED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let stamp = if *TIMED.get_or_init(|| std::env::var("OMNI_LOG_TIME").as_deref() == Ok("1")) {
        let sec = u32::from_le_bytes([rest[2], rest[3], rest[4], rest[5]]);
        let nsec = u32::from_le_bytes([rest[6], rest[7], rest[8], rest[9]]);
        format!("{}.{:03} ", sec % 100_000, nsec / 1_000_000)
    } else {
        String::new()
    };
    let mut out = String::new();
    for line in msg.lines() {
        out += &format!("{stamp}{level}/{tag}({tid:5}): {line}\n");
    }
    if out.is_empty() {
        out = format!("{stamp}{level}/{tag}({tid:5}): \n");
    }
    Some(out)
}

impl Drop for Socket {
    /// The last descriptor of an end is closed: the other end is hung up. A host socket is
    /// released, and the watcher told to let go of it, so the host closes it now.
    fn drop(&mut self) {
        if let Some(Peer::Pair { channel, side }) = &self.peer {
            channel.open[*side].store(false, std::sync::atomic::Ordering::SeqCst);
            crate::poll::notify_key(Arc::as_ptr(channel) as crate::poll::Key);
        }
        if let Some(Peer::Host(host)) = &self.peer {
            host.release();
            self.peer = None;
            crate::hostnet::wake();
        }
    }
}

/// The host socket behind `file`, if it is a TCP or UDP socket.
#[must_use]
pub fn host_of(file: &OpenFile) -> Option<Arc<crate::hostnet::Host>> {
    match &*file.kind.lock() {
        FileKind::Socket(Socket { peer: Some(Peer::Host(h)), .. }) => Some(Arc::clone(h)),
        _ => None,
    }
}

fn nonblocking(file: &OpenFile) -> bool {
    *file.flags.lock() & 0o4000 != 0
}

/// Deliver `bytes` to a connected socket's peer.
pub fn send(socket: &mut Socket, bytes: &[u8]) -> Result<usize, Errno> {
    // To the kernel (`crate::netlink`), from the port the socket is bound to (bound now if not).
    if socket.domain == AF_NETLINK && socket.peer.is_none() {
        let port = match &socket.name {
            Some(name) => u32::from_le_bytes(name[4..8].try_into().expect("4")),
            None => {
                let mut name = vec![0u8; 12];
                name[0..2].copy_from_slice(&(AF_NETLINK as u16).to_le_bytes());
                name[4..8].copy_from_slice(&socket.owner.to_le_bytes());
                socket.name = Some(name);
                socket.owner
            }
        };
        return crate::netlink::send(socket, bytes, port);
    }
    let Socket { peer, inbox, .. } = socket;
    match peer {
        Some(Peer::Dgram(server)) => {
            server.deliver(bytes);
            Ok(bytes.len())
        }
        Some(Peer::Bound(_) | Peer::Inet(_)) => Err(ENOTCONN),
        Some(Peer::Host(h)) => h.try_send(bytes),
        Some(Peer::Dns(proxy)) => Ok(proxy.send(bytes)),
        Some(Peer::Pair { channel, side }) => {
            let other = 1 - *side;
            if !channel.open[other].load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::errno::EPIPE);
            }
            if !channel.stream {
                crate::input_channel::observe(&channel.input, bytes);
            }
            channel.queues.lock()[other].push_back(bytes.to_vec());
            crate::poll::notify_key(Arc::as_ptr(channel) as crate::poll::Key);
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
    // AF_KEY too: key management with no security associations (netd opens and closes one to
    // have the kernel synchronize RCU before it swaps its traffic-stats maps).
    // AF_VSOCK too: the device has its vsock transport, and nothing listens on the host side
    // (connect below), as on a VM with no modem simulator or vsock adb.
    if !matches!(domain, AF_UNIX | AF_INET | AF_INET6 | AF_NETLINK | AF_KEY | AF_VSOCK) {
        return Err(EAFNOSUPPORT);
    }
    let peer = if crate::hostnet::eligible(domain, ty & SOCK_TYPE_MASK, a[2]) {
        Some(Peer::Host(crate::hostnet::Host::create(domain as u16, ty & SOCK_TYPE_MASK == 1, crate::loopns::Namespace::for_process(p))?))
    } else {
        None
    };
    let socket = Socket { domain, ty: ty & SOCK_TYPE_MASK, peer, inbox: std::collections::VecDeque::new(), name: None, protocol: a[2], owner: p.sys.pid as u32, passcred: false };
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

/// Which instance a process is of, for the names sockets are bound to.
fn instance_of(p: &Process) -> usize {
    Arc::as_ptr(p.vfs.binds()) as usize
}

fn sys_connect(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    // A host socket connects without its descriptor's lock held: a blocking connect waits.
    if let Some(host) = host_of(&file) {
        let addr = p.mem.read(a[1], (a[2] as usize).min(128))?;
        return host.connect(&addr, nonblocking(&file), t).map(|()| 0);
    }
    let stderr = match p.fds.get(2).ok().as_deref().map(|f| f.kind.lock().output()) {
        Some(Some(out)) => out,
        _ => Output::Host,
    };
    let mut kind = file.kind.lock();
    let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
    if socket.domain == AF_NETLINK {
        return Ok(0); // to the kernel: what it sends is answered (`crate::netlink`)
    }
    if socket.domain == AF_VSOCK {
        // No host-side listener (a modem simulator, adb over vsock): virtio-vsock resets the
        // connection.
        return Err(crate::errno::ECONNRESET);
    }
    if socket.domain != AF_UNIX {
        // A raw or ICMP socket: the host's network does not stand behind it.
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
    // A socket bound to that name (a service's, init's): connected to it. (logdw and the property
    // service above stay the kernel's own: the log is printed where the runtime shows it.)
    if let Some(server) = crate::unix::Bound::find(instance_of(p), &path) {
        return server.connect(socket, cred_of(p)).map(|()| 0);
    }
    // No netd in this host process (an app's): the kernel answers its DNS proxy. (fwmarkd is not
    // answered: libnetd_client's FwmarkClient::send takes a failed connect as "no error".)
    if path == b"/dev/socket/dnsproxyd" && socket.ty == 1 {
        let hosts = p.vfs.sysroot().read(b"/system/etc/hosts").unwrap_or_default();
        socket.peer = Some(Peer::Dns(crate::dnsproxy::Proxy::new(&hosts)));
        return Ok(0);
    }
    // An abstract name another host process of the instance listens on (`crate::xsocket`).
    if let Some(dir) = p.vfs.binds().instance_dir() {
        if let Some(done) = crate::xsocket::connect(dir, &path, socket, cred_of(p)) {
            return done.map(|()| 0);
        }
    }
    if p.trace {
        eprintln!("[socket] connect {:?}: no service", String::from_utf8_lossy(&path));
    }
    Err(ENOENT)
}

fn sys_sendto(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let bytes = p.mem.read(a[1], (a[2] as usize).min(1 << 20))?;
    if let Some(host) = host_of(&file) {
        let to = if a[4] == 0 { None } else { Some(p.mem.read(a[4], (a[5] as usize).min(128))?) };
        return Ok(host.send(&bytes, to.as_deref(), a[3], nonblocking(&file), t)? as u64);
    }
    let mut kind = file.kind.lock();
    let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
    Ok(send(socket, &bytes)? as u64)
}

fn sys_sendmsg(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
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
    if let Some(host) = host_of(&file) {
        let (name, namelen) = (p.mem.read_u64(a[1])?, p.mem.read_u32(a[1] + 8)?);
        let to = if name == 0 { None } else { Some(p.mem.read(name, (namelen as usize).min(128))?) };
        return Ok(host.send(&bytes, to.as_deref(), a[2], nonblocking(&file), t)? as u64);
    }
    let mut kind = file.kind.lock();
    let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
    Ok(send(socket, &bytes)? as u64)
}

/// `bind`: a netlink socket (a kernel uevent socket: ueventd's, vold's) is bound to its groups
/// under the port the kernel assigns -- the process's pid for its first -- and receives nothing,
/// no device coming or going here. Serving an address of another family is not offered.
fn sys_bind(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let mut kind = file.kind.lock();
    let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
    if socket.domain == AF_UNIX {
        let name = unix_path(p, a[1], a[2])?;
        let bound = crate::unix::Bound::bind(instance_of(p), &name, socket.ty, cred_of(p))?;
        socket.name = Some(p.mem.read(a[1], a[2] as usize)?);
        socket.peer = Some(Peer::Bound(bound));
        return Ok(0);
    }
    if let Some(Peer::Host(host)) = &socket.peer {
        let addr = p.mem.read(a[1], (a[2] as usize).min(28))?;
        return host.bind(&addr).map(|()| 0);
    }
    if matches!(socket.domain, AF_INET | AF_INET6) {
        if socket.name.is_some() {
            return Err(EINVAL);
        }
        let addr = p.mem.read(a[1], (a[2] as usize).min(28))?;
        let port = crate::inet::bind(instance_of(p), socket.domain as u16, socket.ty, &addr)?;
        socket.name = Some(port.name.clone());
        socket.peer = Some(Peer::Inet(port));
        return Ok(0);
    }
    if socket.domain != AF_NETLINK {
        p.refusals.record(format!("bind: family {}", socket.domain), t.pc, t.lr);
        return Err(crate::errno::ENOSYS);
    }
    if a[2] < 12 {
        return Err(EINVAL);
    }
    let addr = p.mem.read(a[1], 12)?;
    if u16::from_le_bytes([addr[0], addr[1]]) as u64 != AF_NETLINK {
        return Err(EINVAL);
    }
    if socket.name.is_some() {
        return Err(EINVAL);
    }
    let port = match u32::from_le_bytes(addr[4..8].try_into().expect("4")) {
        0 => p.sys.pid as u32,
        port => port,
    };
    let mut name = addr;
    name[4..8].copy_from_slice(&port.to_le_bytes());
    socket.name = Some(name);
    Ok(0)
}

/// `listen`: a bound stream or seqpacket socket takes connections.
fn sys_listen(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let kind = file.kind.lock();
    let FileKind::Socket(socket) = &*kind else { return Err(crate::errno::ENOTSOCK) };
    match &socket.peer {
        Some(Peer::Bound(bound)) if bound.ty != 2 => {
            bound.listening.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(dir) = p.vfs.binds().instance_dir() {
                crate::xsocket::publish(dir, &bound.name);
            }
            Ok(0)
        }
        Some(Peer::Host(host)) => host.listen(a[1] as i64 as i32).map(|()| 0),
        _ => Err(EINVAL),
    }
}

fn sys_accept(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    sys_accept4(p, t, [a[0], a[1], a[2], 0, 0, 0])
}

/// `accept4(fd, addr, addrlen, flags)`: the next connection (waiting for one unless
/// non-blocking), as a new descriptor; the peer's address is unnamed.
fn sys_accept4(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    if a[3] & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    if let Some(host) = host_of(&file) {
        let (conn, peer) = host.accept(nonblocking(&file), t)?;
        let socket = Socket { domain: u64::from(host.domain), ty: 1, peer: Some(Peer::Host(conn)), inbox: std::collections::VecDeque::new(), name: None, protocol: 0, owner: p.sys.pid as u32, passcred: false };
        let flags = if a[3] & SOCK_NONBLOCK != 0 { 0o4000 } else { 0 } | 2;
        let new = OpenFile { kind: Mutex::new(FileKind::Socket(socket)), flags: Mutex::new(flags) };
        let fd = p.fds.insert(Arc::new(new), a[3] & SOCK_CLOEXEC != 0, 0)?;
        if a[1] != 0 && a[2] != 0 {
            write_name(p, a[1], a[2], &peer)?;
        }
        return Ok(fd as u64);
    }
    let bound = match &*file.kind.lock() {
        FileKind::Socket(Socket { peer: Some(Peer::Bound(b)), .. }) if b.listening.load(std::sync::atomic::Ordering::SeqCst) => Arc::clone(b),
        FileKind::Socket(_) => return Err(EINVAL),
        _ => return Err(crate::errno::ENOTSOCK),
    };
    let nonblocking = *file.flags.lock() & 0o4000 != 0;
    let socket = loop {
        let seen = crate::poll::generation();
        if let Some(s) = bound.accept() {
            break s;
        }
        if nonblocking {
            return Err(crate::errno::EAGAIN);
        }
        crate::poll::wait_for_change(seen, None, t)?;
    };
    if a[1] != 0 && a[2] != 0 {
        p.mem.write(a[1], &(AF_UNIX as u16).to_le_bytes())?;
        p.mem.write_u32(a[2], 2)?;
    }
    let flags = if a[3] & SOCK_NONBLOCK != 0 { 0o4000 } else { 0 } | 2;
    let file = OpenFile { kind: Mutex::new(FileKind::Socket(socket)), flags: Mutex::new(flags) };
    Ok(p.fds.insert(Arc::new(file), a[3] & SOCK_CLOEXEC != 0, 0)? as u64)
}

/// A socket address into the caller's buffer as `getsockname` returns one: as much as fits in
/// `*len_at`, and `*len_at` set to its full length.
fn write_name(p: &Process, at: u64, len_at: u64, name: &[u8]) -> Result<(), Errno> {
    let room = u32::from_le_bytes(p.mem.read(len_at, 4)?.try_into().expect("4")) as usize;
    p.mem.write(at, &name[..name.len().min(room)])?;
    p.mem.write(len_at, &(name.len() as u32).to_le_bytes())
}

/// `getpeername`: a host socket's peer; a socket pair's other end (unnamed: the family alone).
fn sys_getpeername(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    if let Some(host) = host_of(&file) {
        let name = host.peer()?;
        return write_name(p, a[1], a[2], &name).map(|()| 0);
    }
    let name = match &*file.kind.lock() {
        FileKind::Socket(Socket { peer: None | Some(Peer::Bound(_) | Peer::Inet(_)), domain, .. }) if *domain != AF_NETLINK => return Err(ENOTCONN),
        // Netlink's peer is the kernel: `sockaddr_nl` with port 0.
        FileKind::Socket(s) if s.domain == AF_NETLINK => [&(AF_NETLINK as u16).to_le_bytes()[..], &[0; 10]].concat(),
        FileKind::Socket(s) => (s.domain as u16).to_le_bytes().to_vec(),
        _ => return Err(crate::errno::ENOTSOCK),
    };
    write_name(p, a[1], a[2], &name).map(|()| 0)
}

/// `getsockname`: the bound address; unbound, the family alone.
fn sys_getsockname(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    if let Some(host) = host_of(&file) {
        let name = host.name()?;
        return write_name(p, a[1], a[2], &name).map(|()| 0);
    }
    let kind = file.kind.lock();
    let FileKind::Socket(socket) = &*kind else { return Err(crate::errno::ENOTSOCK) };
    let name = match &socket.name {
        Some(name) => name.clone(),
        None if matches!(socket.domain, AF_INET | AF_INET6) => crate::inet::unbound_name(socket.domain as u16),
        None => (socket.domain as u16).to_le_bytes().to_vec(),
    };
    let room = u32::from_le_bytes(p.mem.read(a[2], 4)?.try_into().expect("4")) as usize;
    p.mem.write(a[1], &name[..name.len().min(room)])?;
    p.mem.write(a[2], &(name.len() as u32).to_le_bytes())?;
    Ok(0)
}

/// Options are accepted and not acted on: timeouts, buffer sizes and credentials change nothing
/// for a socket with no peer or with `logd`.
fn sys_setsockopt(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let is_socket = matches!(&*file.kind.lock(), FileKind::Socket(_));
    if !is_socket {
        return Err(crate::errno::ENOTSOCK);
    }
    if let Some(host) = host_of(&file) {
        let value = p.mem.read(a[3], (a[4] as usize).min(256))?;
        if host.set_option(a[1], a[2], &value)? {
            return Ok(0);
        }
        if p.trace {
            eprintln!("[socket] setsockopt level {} name {}: accepted, not acted on", a[1], a[2]);
        }
    }
    // SO_PASSCRED.
    if a[1] == 1 && a[2] == 16 && a[4] >= 4 {
        let on = p.mem.read_u32(a[3])? != 0;
        if let FileKind::Socket(s) = &mut *file.kind.lock() {
            s.passcred = on;
            // A listening socket's connections inherit it.
            if let Some(Peer::Bound(b)) = &s.peer {
                b.passcred.store(on, std::sync::atomic::Ordering::SeqCst);
            }
        }
        return Ok(0);
    }
    // xtables, on a raw socket (iptables).
    if matches!(a[1], crate::xtables::SOL_IP | crate::xtables::SOL_IPV6) && matches!(a[2], 64 | 65) {
        return crate::xtables::set(p, a[1], a[2], a[3], a[4] as usize);
    }
    Ok(0)
}

fn sys_shutdown(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    if let Some(host) = host_of(&file) {
        return host.shutdown(a[1]).map(|()| 0);
    }
    let is_socket = matches!(&*file.kind.lock(), FileKind::Socket(_));
    if is_socket { Ok(0) } else { Err(crate::errno::ENOTSOCK) }
}

/// Take what the peer sent back: all of it that fits, or `EAGAIN` when nothing is there. From a
/// socket pair: one message (the rest of it discarded, as a datagram's is) or a stream's bytes;
/// end of file (0) once the other end is closed and nothing is left.
pub fn receive(socket: &mut Socket, buf: &mut [u8]) -> Result<usize, Errno> {
    if let Some(Peer::Host(host)) = &socket.peer {
        return host.try_recv(buf);
    }
    if let Some(Peer::Dns(proxy)) = &socket.peer {
        return proxy.receive(buf);
    }
    if let Some(Peer::Bound(bound)) = &socket.peer {
        return bound.receive(buf).ok_or(crate::errno::EAGAIN);
    }
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

/// Whether `socket` is a socket pair's end (one `crate::relay` can carry to another host process).
#[must_use]
pub fn is_pair(socket: &Socket) -> bool {
    matches!(socket.peer, Some(Peer::Pair { .. }))
}

/// For `crate::relay`: the next message a pair's end was sent (a stream's queued bytes), without
/// waiting.
pub fn take(socket: &Socket) -> crate::relay::Took {
    let Some(Peer::Pair { channel, side }) = &socket.peer else { return crate::relay::Took::Closed };
    let mut queues = channel.queues.lock();
    let q = &mut queues[*side];
    if let Some(msg) = q.pop_front() {
        let mut msg = msg;
        if channel.stream {
            while let Some(more) = q.pop_front() {
                msg.extend_from_slice(&more);
            }
        }
        return crate::relay::Took::Data(msg);
    }
    if channel.open[1 - *side].load(std::sync::atomic::Ordering::SeqCst) {
        crate::relay::Took::Nothing
    } else {
        crate::relay::Took::Closed
    }
}

/// `getsockopt`: the few options callers read. `SO_PEERCRED` reports the peer as init (a local
/// service's client-credential check then passes); `SO_TYPE` the socket's type; `SO_ERROR` none;
/// buffer sizes a plausible value. Everything else is zeroed.
fn sys_getsockopt(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    if let Some(host) = host_of(&file) {
        if let Some(bytes) = host.get_option(a[1], a[2])? {
            let cap = p.mem.read_u32(a[4])? as usize;
            let n = bytes.len().min(cap);
            p.mem.write(a[3], &bytes[..n])?;
            p.mem.write_u32(a[4], n as u32)?;
            return Ok(0);
        }
    }
    let (ty, peer_cred) = match &*file.kind.lock() {
        FileKind::Socket(s) => (s.ty, peer_cred(s)),
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
    // xtables, on a raw socket (iptables).
    if matches!(level, crate::xtables::SOL_IP | crate::xtables::SOL_IPV6) && (64..=67).contains(&name) {
        let room = p.mem.read_u32(len_ptr)? as usize;
        let bytes = crate::xtables::get(p, level, name, val, room)?;
        return write(&bytes);
    }
    const SOL_SOCKET: u64 = 1;
    if level == SOL_SOCKET {
        match name {
            17 => {
                // SO_PEERCRED: struct ucred { pid, uid, gid } -- the peer's; a socket connected to
                // none of this runtime's processes (logd, the property service) reports init.
                return write(&ucred(peer_cred.unwrap_or([1, 0, 0])));
            }
            3 => return write(&(ty as u32).to_le_bytes()), // SO_TYPE
            4 => return write(&0u32.to_le_bytes()),        // SO_ERROR
            7 | 8 => return write(&(256 * 1024u32).to_le_bytes()), // SO_SNDBUF/RCVBUF
            _ => return write(&0u32.to_le_bytes()),
        }
    }
    write(&0u32.to_le_bytes())
}

/// The credentials of the process at a socket's other end, when it is one of this runtime's.
fn peer_cred(s: &Socket) -> Option<Cred> {
    match &s.peer {
        Some(Peer::Pair { channel, side }) => Some(channel.creds[1 - *side]),
        Some(Peer::Dgram(bound)) => Some(bound.cred),
        _ => None,
    }
}

/// `struct ucred`'s bytes.
fn ucred(c: Cred) -> [u8; 12] {
    let mut out = [0u8; 12];
    for (i, v) in c.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

/// `MSG_DONTWAIT`.
const MSG_DONTWAIT: u64 = 0x40;

fn sys_recvfrom(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let mut buf = crate::zbuf::ZeroBuf::new((a[2] as usize).min(1 << 20));
    if let Some(host) = host_of(&file) {
        let (n, from) = host.recv(&mut buf, a[3], nonblocking(&file), t)?;
        p.mem.write(a[1], &buf[..n])?;
        if a[4] != 0 && a[5] != 0 {
            match from {
                Some(addr) => write_name(p, a[4], a[5], &crate::hostnet::sockaddr(&addr))?,
                None => p.mem.write_u32(a[5], 0)?,
            }
        }
        return Ok(n as u64);
    }
    let n = receive_waiting(&file, &mut buf, a[3] & MSG_DONTWAIT != 0, t)?;
    p.mem.write(a[1], &buf[..n])?;
    Ok(n as u64)
}

/// Receive from `file`, waiting (unless it or the call is non-blocking) while a socket pair's end
/// has nothing to read and its other end is open.
fn receive_waiting(file: &OpenFile, buf: &mut [u8], dontwait: bool, t: &Task) -> Result<usize, Errno> {
    loop {
        let watch = crate::poll::watch(crate::poll::key_of(file).map(|k| vec![k]));
        let (r, pair) = {
            let mut kind = file.kind.lock();
            let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
            (receive(socket, buf), matches!(socket.peer, Some(Peer::Pair { .. } | Peer::Bound(_) | Peer::Dns(_))) || socket.domain == AF_NETLINK)
        };
        let nonblocking = dontwait || *file.flags.lock() & 0o4000 != 0;
        match r {
            Err(e) if e == crate::errno::EAGAIN && pair && !nonblocking => watch.wait(None, t)?,
            other => return other,
        }
    }
}

/// What a change to this socket is told by: its pair's channel, its host socket, the bound socket
/// it listens or receives on; `None` for the rest (their changes are told to everyone).
#[must_use]
pub fn key(socket: &Socket) -> Option<crate::poll::Key> {
    match &socket.peer {
        Some(Peer::Pair { channel, .. }) => Some(Arc::as_ptr(channel) as crate::poll::Key),
        Some(Peer::Host(h)) => Some(Arc::as_ptr(h) as crate::poll::Key),
        Some(Peer::Bound(b)) => Some(Arc::as_ptr(b) as crate::poll::Key),
        _ => None,
    }
}

/// `read` of a socket pair's end (which may wait); `None` for anything else.
pub fn read(file: &OpenFile, buf: &mut [u8], t: &Task) -> Option<Result<usize, Errno>> {
    if let Some(host) = host_of(file) {
        return Some(host.recv(buf, 0, nonblocking(file), t).map(|(n, _)| n));
    }
    let pair = matches!(&*file.kind.lock(), FileKind::Socket(Socket { peer: Some(Peer::Pair { .. } | Peer::Dns(_)), .. }));
    pair.then(|| receive_waiting(file, buf, false, t))
}

/// `write` of a host socket (which may wait; `EPIPE` raises `SIGPIPE`); `None` for anything else.
pub fn write(file: &OpenFile, bytes: &[u8], t: &Task) -> Option<Result<usize, Errno>> {
    let host = host_of(file)?;
    Some(host.send(bytes, None, 0, nonblocking(file), t))
}

/// Bytes ready to read (`FIONREAD`): a datagram's the next message's, a stream's all queued.
#[must_use]
pub fn available(socket: &Socket) -> usize {
    if let Some(Peer::Host(host)) = &socket.peer {
        return host.available();
    }
    if let Some(Peer::Dns(proxy)) = &socket.peer {
        return proxy.available();
    }
    if let Some(Peer::Pair { channel, side }) = &socket.peer {
        let queues = channel.queues.lock();
        let q = &queues[*side];
        return if channel.stream { q.iter().map(Vec::len).sum() } else { q.front().map_or(0, Vec::len) };
    }
    socket.inbox.len()
}

/// What a socket is ready for: a pair's end is readable with something queued, writable while the
/// other end is open, and hung up (readable, `POLLHUP`) when it is closed.
#[must_use]
pub fn readiness(socket: &Socket) -> u32 {
    const IN: u32 = 0x1;
    const OUT: u32 = 0x4;
    const HUP: u32 = 0x10;
    match &socket.peer {
        Some(Peer::Host(host)) => host.readiness(),
        Some(Peer::Dns(proxy)) => (if proxy.available() > 0 { IN } else { 0 }) | OUT,
        Some(Peer::Bound(bound)) => if bound.ready() { IN } else { 0 },
        Some(Peer::Dgram(_)) => OUT,
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
        creds: [cred_of(p), cred_of(p)],
        queues: Mutex::new([std::collections::VecDeque::new(), std::collections::VecDeque::new()]),
        open: [std::sync::atomic::AtomicBool::new(true), std::sync::atomic::AtomicBool::new(true)],
        stream: kind == 1,
        input: Mutex::default(),
    });
    let flags = if ty & SOCK_NONBLOCK != 0 { 0o4000 } else { 0 } | 2; // O_RDWR
    let mut fds = [0i32; 2];
    for (side, fd) in fds.iter_mut().enumerate() {
        let socket = Socket { domain, ty: kind, peer: Some(Peer::Pair { channel: Arc::clone(&channel), side }), inbox: std::collections::VecDeque::new(), name: None, protocol: 0, owner: p.sys.pid as u32, passcred: false };
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
    let mut buf = crate::zbuf::ZeroBuf::new(total.min(1 << 20));
    let host = host_of(&file);
    let (n, from) = match &host {
        Some(host) => host.recv(&mut buf, a[2], nonblocking(&file), t)?,
        None => (receive_waiting(&file, &mut buf, a[2] & MSG_DONTWAIT != 0, t)?, None),
    };
    let mut at = 0;
    for (base, len) in spans {
        if at >= n {
            break;
        }
        let k = len.min(n - at);
        p.mem.write(base, &buf[at..at + k])?;
        at += k;
    }
    if host.is_some() {
        // msg_name: where a datagram came from (a stream's is not reported: namelen 0).
        let name = p.mem.read_u64(a[1])?;
        if name != 0 {
            match from {
                Some(addr) => write_name(p, name, a[1] + 8, &crate::hostnet::sockaddr(&addr))?,
                None => p.mem.write_u32(a[1] + 8, 0)?,
            }
        }
        p.mem.write(a[1] + 40, &0u64.to_le_bytes())?;
        p.mem.write(a[1] + 48, &0u32.to_le_bytes())?;
        return Ok(n as u64);
    }
    // SO_PASSCRED: the sender's credentials, SCM_CREDENTIALS; otherwise no control data.
    let creds = match &*file.kind.lock() {
        FileKind::Socket(s) if s.passcred => peer_cred(s),
        _ => None,
    };
    let (control, room) = (p.mem.read_u64(a[1] + 32)?, p.mem.read_u64(a[1] + 40)?);
    let mut controllen = 0u64;
    let mut flags = 0u32;
    if let Some(c) = creds {
        // struct cmsghdr { len u64, level i32, type i32 } + struct ucred, padded to 8.
        const SCM_CREDENTIALS: u32 = 2;
        if room >= 32 {
            let mut cmsg = Vec::with_capacity(32);
            cmsg.extend_from_slice(&28u64.to_le_bytes());
            cmsg.extend_from_slice(&1u32.to_le_bytes());
            cmsg.extend_from_slice(&SCM_CREDENTIALS.to_le_bytes());
            cmsg.extend_from_slice(&ucred(c));
            cmsg.extend_from_slice(&[0; 4]);
            p.mem.write(control, &cmsg)?;
            controllen = 32;
        } else {
            flags |= 0x8; // MSG_CTRUNC
        }
    }
    p.mem.write(a[1] + 40, &controllen.to_le_bytes())?;
    p.mem.write(a[1] + 48, &flags.to_le_bytes())?;
    Ok(n as u64)
}

pub fn install(table: &mut Table) {
    table.set(nr::RECVFROM, sys_recvfrom);
    table.set(nr::RECVMSG, sys_recvmsg);
    table.set(nr::SOCKETPAIR, sys_socketpair);
    table.set(nr::SOCKET, sys_socket);
    table.set(nr::BIND, sys_bind);
    table.set(nr::LISTEN, sys_listen);
    table.set(nr::ACCEPT, sys_accept);
    table.set(nr::ACCEPT4, sys_accept4);
    table.set(nr::GETSOCKNAME, sys_getsockname);
    table.set(nr::GETPEERNAME, sys_getpeername);
    table.set(nr::GETSOCKOPT, sys_getsockopt);
    table.set(nr::CONNECT, sys_connect);
    table.set(nr::SENDTO, sys_sendto);
    table.set(nr::SENDMSG, sys_sendmsg);
    table.set(nr::SETSOCKOPT, sys_setsockopt);
    table.set(nr::SHUTDOWN, sys_shutdown);
}
