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
    /// `sent[side]`, `taken[side]`: the bytes ever queued to `side` and taken from it (a datagram
    /// counts whole when taken) -- the stream positions descriptors are delivered at. Changed with
    /// `queues` held.
    sent: [std::sync::atomic::AtomicU64; 2],
    taken: [std::sync::atomic::AtomicU64; 2],
    /// `rights[side]`: descriptors sent to `side` with a message (`SCM_RIGHTS`), each batch at the
    /// position of its message's first byte. Chromium's IPC (Mojo) passes every shared-memory
    /// region and channel this way, between an app and its WebView renderer; when they were
    /// dropped, the renderer read a message missing its handles, took its channel for broken and
    /// exited, and the WebView killed the app ("Renderer process crash detected", run 2026-10-12).
    rights: Mutex<[std::collections::VecDeque<(u64, Vec<Arc<OpenFile>>)>; 2]>,
}

impl PairChannel {
    fn new(creds: [Cred; 2], stream: bool) -> Self {
        Self {
            creds,
            queues: Mutex::new([std::collections::VecDeque::new(), std::collections::VecDeque::new()]),
            open: [std::sync::atomic::AtomicBool::new(true), std::sync::atomic::AtomicBool::new(true)],
            stream,
            input: Mutex::default(),
            sent: [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)],
            taken: [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)],
            rights: Mutex::new([std::collections::VecDeque::new(), std::collections::VecDeque::new()]),
        }
    }

    /// The descriptors `side` has now read up to (sent with a message whose first byte it took).
    fn rights_taken(&self, side: usize) -> Vec<Arc<OpenFile>> {
        let taken = self.taken[side].load(std::sync::atomic::Ordering::SeqCst);
        let mut rights = self.rights.lock();
        let mut out = Vec::new();
        while rights[side].front().is_some_and(|(at, _)| *at < taken) {
            out.extend(rights[side].pop_front().expect("checked").1);
        }
        out
    }
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
    let channel = Arc::new(PairChannel::new([client, server], ty == 1));
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
    send_with(socket, bytes, Vec::new())
}

/// Deliver `bytes` with descriptors (`SCM_RIGHTS`) to a connected socket's peer. Only a socket
/// pair's end carries descriptors (as only an `AF_UNIX` socket does); the others drop them.
pub fn send_with(socket: &mut Socket, bytes: &[u8], rights: Vec<Arc<OpenFile>>) -> Result<usize, Errno> {
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
    let own = own_key(socket);
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
            let mut queues = channel.queues.lock();
            let at = channel.sent[other].fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::SeqCst);
            if !rights.is_empty() {
                channel.rights.lock()[other].push_back((at, rights));
            }
            queues[other].push_back(bytes.to_vec());
            drop(queues);
            crate::poll::notify_key(Arc::as_ptr(channel) as crate::poll::Key);
            Ok(bytes.len())
        }
        Some(Peer::PropertyService { pending, service }) => {
            pending.extend_from_slice(bytes);
            let service = std::sync::Arc::clone(service);
            property_requests(pending, &service, inbox);
            if crate::poll::KEYED.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(key) = own {
                    crate::poll::notify_key(key);
                }
            }
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
    let was = peerless_key(&file);
    let done = connect(p, t, a, &file);
    told_of_peer(&file, was);
    done
}

fn connect(p: &Process, t: &mut Task, a: [u64; 6], file: &Arc<OpenFile>) -> SysResult {
    let file = Arc::clone(file);
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
    // struct msghdr: name, namelen, iov, iovlen, control, controllen, flags -- read at once.
    let hdr = Msghdr::read(p, a[1])?;
    if hdr.iovlen > 1024 {
        return Err(EINVAL);
    }
    // The buffers' total, capped at 1 MiB as before, gathered into this thread's scratch buffer
    // when small (an input channel's message: no allocation).
    let mut spans = Spans::default();
    let mut total = 0usize;
    for i in 0..hdr.iovlen {
        let (base, len) = crate::fd::iovec(p, hdr.iov + i * 16)?;
        let take = len.min((1 << 20) - total);
        spans.push((base, take));
        total += take;
    }
    let rights = rights_sent(p, &hdr)?;
    crate::guest::with_scratch(total, |bytes| {
        let mut at = 0;
        for &(base, len) in spans.as_slice() {
            p.mem.read_into(base, &mut bytes[at..at + len])?;
            at += len;
        }
        let bytes = &*bytes;
        if let Some(host) = host_of(&file) {
            let namelen = u32::try_from(hdr.namelen).unwrap_or(u32::MAX);
            let to = if hdr.name == 0 { None } else { Some(p.mem.read(hdr.name, (namelen as usize).min(128))?) };
            return Ok(host.send(bytes, to.as_deref(), a[2], nonblocking(&file), t)? as u64);
        }
        let mut kind = file.kind.lock();
        let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
        Ok(send_with(socket, bytes, rights)? as u64)
    })
}

/// `SOL_SOCKET`, and its control messages' types.
const SOL_SOCKET: u32 = 1;
const SCM_RIGHTS: u32 = 1;
const SCM_CREDENTIALS: u32 = 2;

/// The descriptors a `sendmsg` passes (its `SCM_RIGHTS` control messages), as open files.
fn rights_sent(p: &Process, hdr: &Msghdr) -> Result<Vec<Arc<OpenFile>>, Errno> {
    let mut rights = Vec::new();
    if hdr.control == 0 || hdr.controllen < 16 {
        return Ok(rights);
    }
    let control = p.mem.read(hdr.control, (hdr.controllen as usize).min(64 * 1024))?;
    // struct cmsghdr { len u64, level i32, type i32 } then the data, each padded to 8.
    let mut at = 0usize;
    while at + 16 <= control.len() {
        let len = u64::from_le_bytes(control[at..at + 8].try_into().expect("8")) as usize;
        if len < 16 || at + len > control.len() {
            break;
        }
        let level = u32::from_le_bytes(control[at + 8..at + 12].try_into().expect("4"));
        let ty = u32::from_le_bytes(control[at + 12..at + 16].try_into().expect("4"));
        if level == SOL_SOCKET && ty == SCM_RIGHTS {
            for fd in control[at + 16..at + len].chunks_exact(4) {
                rights.push(p.fds.get(i32::from_le_bytes(fd.try_into().expect("4")))?);
            }
        }
        at += (len + 7) & !7;
    }
    Ok(rights)
}

/// A guest `struct msghdr` (56 bytes), read in one copy.
struct Msghdr {
    name: u64,
    namelen: u64,
    iov: u64,
    iovlen: u64,
    control: u64,
    controllen: u64,
}

impl Msghdr {
    fn read(p: &Process, at: u64) -> Result<Self, Errno> {
        let b: [u8; 56] = p.mem.read_array(at)?;
        let q = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().expect("8"));
        Ok(Self { name: q(0), namelen: u64::from(u32::from_le_bytes(b[8..12].try_into().expect("4"))), iov: q(16), iovlen: q(24), control: q(32), controllen: q(40) })
    }
}

/// A message's buffers: up to 8 on the stack (a message has one or two), more in a `Vec`.
#[derive(Default)]
struct Spans {
    small: [(u64, usize); 8],
    n: usize,
    more: Vec<(u64, usize)>,
}

impl Spans {
    fn push(&mut self, span: (u64, usize)) {
        if self.n < self.small.len() && self.more.is_empty() {
            self.small[self.n] = span;
            self.n += 1;
        } else {
            if self.more.is_empty() {
                self.more.extend_from_slice(&self.small[..self.n]);
            }
            self.more.push(span);
        }
    }

    fn as_slice(&self) -> &[(u64, usize)] {
        if self.more.is_empty() { &self.small[..self.n] } else { &self.more }
    }
}

/// `bind`: a netlink socket (a kernel uevent socket: ueventd's, vold's) is bound to its groups
/// under the port the kernel assigns -- the process's pid for its first -- and receives nothing,
/// no device coming or going here. Serving an address of another family is not offered.
fn sys_bind(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let was = peerless_key(&file);
    let done = bind(p, t, a, &file);
    told_of_peer(&file, was);
    done
}

fn bind(p: &Process, t: &mut Task, a: [u64; 6], file: &Arc<OpenFile>) -> SysResult {
    let file = Arc::clone(file);
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
        // `poll_keyed`: a connection to this socket (`Bound::connect` tells its key), not every
        // change in the process.
        let keyed = (!nonblocking && crate::poll::KEYED.load(std::sync::atomic::Ordering::Relaxed)).then(|| crate::poll::watch(Some(vec![Arc::as_ptr(&bound) as crate::poll::Key])));
        let seen = crate::poll::generation();
        if let Some(s) = bound.accept() {
            break s;
        }
        if nonblocking {
            return Err(crate::errno::EAGAIN);
        }
        match &keyed {
            Some(watch) => watch.wait(None, t)?,
            None => crate::poll::wait_for_change(seen, None, t)?,
        }
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
            channel.taken[*side].fetch_add(msg.len() as u64, std::sync::atomic::Ordering::SeqCst);
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
        channel.taken[*side].fetch_add(n as u64, std::sync::atomic::Ordering::SeqCst);
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
        channel.taken[*side].fetch_add(msg.len() as u64, std::sync::atomic::Ordering::SeqCst);
        drop(queues);
        let rights = channel.rights_taken(*side);
        return crate::relay::Took::Data(msg, rights);
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
    // Looked at first, registered only to wait (then looked at again): a receive that finds a
    // message takes no queue lock to register and allocates nothing.
    let mut watching: Option<crate::poll::Watch> = None;
    loop {
        let (r, pair) = {
            let mut kind = file.kind.lock();
            let FileKind::Socket(socket) = &mut *kind else { return Err(crate::errno::ENOTSOCK) };
            (receive(socket, buf), matches!(socket.peer, Some(Peer::Pair { .. } | Peer::Bound(_) | Peer::Dns(_))) || socket.domain == AF_NETLINK)
        };
        let nonblocking = dontwait || *file.flags.lock() & 0o4000 != 0;
        match r {
            Err(e) if e == crate::errno::EAGAIN && pair && !nonblocking => match watching.take() {
                None => watching = Some(crate::poll::watch(crate::poll::key_of(file).map(|k| vec![k]))),
                Some(w) => w.wait(None, t)?,
            },
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
        // Its answers are told by the proxy (`dnsproxy`), as they are queued.
        Some(Peer::Dns(d)) => Some(Arc::as_ptr(d) as crate::poll::Key),
        _ => None,
    }
}

/// The key of a socket whose readiness only its own sends change (`crate::poll::KEYED`): a netlink
/// socket (the kernel answers into its inbox as it is sent to), init's property service (answered
/// as it is sent to), `logd`'s and a datagram socket connected to a bound one (always writable,
/// never readable). The socket's own address, which is stable: it lives in its open file.
#[must_use]
///
/// A socket of another family with no peer yet (a raw or ICMP socket -- the host's network does not
/// stand behind those -- or one not yet bound or connected) is keyed so too: nothing fills its inbox
/// but its own sends, and the `bind` or `connect` that gives it a peer wakes its waiters
/// ([`told_of_peer`]) to look again under the peer's key. Waiting on anything instead, the system
/// host's ConnectivityService thread (an IPv6 raw socket in its looper) was woken by every change
/// in the process, ~3,200 times a second (M4, 2026-10-16).
pub fn own_key(socket: &Socket) -> Option<crate::poll::Key> {
    let own = match &socket.peer {
        None => true,
        Some(Peer::Logd(_) | Peer::PropertyService { .. } | Peer::Dgram(_)) => true,
        _ => false,
    };
    own.then_some(std::ptr::from_ref(socket) as crate::poll::Key)
}

/// The socket's own key while it has no peer: what a `bind` or `connect` that gives it one tells.
fn peerless_key(file: &OpenFile) -> Option<crate::poll::Key> {
    match &*file.kind.lock() {
        FileKind::Socket(s) if s.peer.is_none() => Some(std::ptr::from_ref(s) as crate::poll::Key),
        _ => None,
    }
}

/// After `bind`/`connect`: a socket that had no peer and has one now wakes whoever waited on its
/// own key ([`own_key`]), to look again under its new one.
fn told_of_peer(file: &OpenFile, was: Option<crate::poll::Key>) {
    if let Some(key) = was {
        if peerless_key(file).is_none() {
            crate::poll::notify_key(key);
        }
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
    let channel = Arc::new(PairChannel::new([cred_of(p), cred_of(p)], kind == 1));
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

/// `recvmsg`: into the message's iovecs; the control data is the sender's credentials
/// (`SO_PASSCRED`) and the descriptors sent with what was read (`SCM_RIGHTS`).
fn sys_recvmsg(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(a[0] as i64 as i32)?;
    let hdr = Msghdr::read(p, a[1])?;
    if hdr.iovlen > 1024 {
        return Err(EINVAL);
    }
    let mut spans = Spans::default();
    let mut total = 0usize;
    for i in 0..hdr.iovlen {
        let span = crate::fd::iovec(p, hdr.iov + i * 16)?;
        spans.push(span);
        total = total.saturating_add(span.1);
    }
    let host = host_of(&file);
    // Into this thread's scratch buffer when small: only the `n` bytes received are used.
    let (n, from) = crate::guest::with_scratch(total.min(1 << 20), |buf| -> Result<_, Errno> {
        let (n, from) = match &host {
            Some(host) => host.recv(buf, a[2], nonblocking(&file), t)?,
            None => (receive_waiting(&file, buf, a[2] & MSG_DONTWAIT != 0, t)?, None),
        };
        let mut at = 0;
        for &(base, len) in spans.as_slice() {
            if at >= n {
                break;
            }
            let k = len.min(n - at);
            p.mem.write(base, &buf[at..at + k])?;
            at += k;
        }
        Ok((n, from))
    })?;
    if host.is_some() {
        // msg_name: where a datagram came from (a stream's is not reported: namelen 0).
        let name = hdr.name;
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
    // SO_PASSCRED: the sender's credentials, SCM_CREDENTIALS; the descriptors sent with what was
    // read, SCM_RIGHTS.
    let (creds, rights) = match &*file.kind.lock() {
        FileKind::Socket(s) => (if s.passcred { peer_cred(s) } else { None }, match &s.peer {
            Some(Peer::Pair { channel, side }) => channel.rights_taken(*side),
            _ => Vec::new(),
        }),
        _ => (None, Vec::new()),
    };
    let (control, room) = (hdr.control, hdr.controllen);
    let mut cmsgs = Vec::new();
    let mut flags = 0u32;
    if let Some(c) = creds {
        // struct cmsghdr { len u64, level i32, type i32 } + struct ucred, padded to 8.
        if room >= 32 {
            cmsgs.extend_from_slice(&28u64.to_le_bytes());
            cmsgs.extend_from_slice(&SOL_SOCKET.to_le_bytes());
            cmsgs.extend_from_slice(&SCM_CREDENTIALS.to_le_bytes());
            cmsgs.extend_from_slice(&ucred(c));
            cmsgs.extend_from_slice(&[0; 4]);
        } else {
            flags |= 0x8; // MSG_CTRUNC
        }
    }
    if !rights.is_empty() {
        // As many as the buffer holds, each a new descriptor here (MSG_CMSG_CLOEXEC: close-on-exec);
        // the rest are closed, and the message says it was cut (MSG_CTRUNC).
        let fit = ((room as usize).saturating_sub(cmsgs.len() + 16) / 4).min(rights.len());
        if fit < rights.len() {
            flags |= 0x8;
        }
        if fit > 0 {
            const MSG_CMSG_CLOEXEC: u64 = 0x4000_0000;
            let mut fds = Vec::with_capacity(fit);
            for file in rights.into_iter().take(fit) {
                fds.push(p.fds.insert(file, a[2] & MSG_CMSG_CLOEXEC != 0, 0)?);
            }
            let len = 16 + 4 * fds.len();
            cmsgs.extend_from_slice(&(len as u64).to_le_bytes());
            cmsgs.extend_from_slice(&SOL_SOCKET.to_le_bytes());
            cmsgs.extend_from_slice(&SCM_RIGHTS.to_le_bytes());
            for fd in fds {
                cmsgs.extend_from_slice(&fd.to_le_bytes());
            }
            cmsgs.resize((cmsgs.len() + 7) & !7, 0);
        }
    }
    let controllen = (cmsgs.len() as u64).min(room);
    if controllen > 0 {
        p.mem.write(control, &cmsgs[..controllen as usize])?;
    }
    // msg_controllen and msg_flags, adjacent: one copy.
    let mut tail = [0u8; 12];
    tail[..8].copy_from_slice(&controllen.to_le_bytes());
    tail[8..].copy_from_slice(&flags.to_le_bytes());
    p.mem.write(a[1] + 40, &tail)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn end(peer: Peer) -> Socket {
        Socket { domain: AF_UNIX, ty: 1, peer: Some(peer), inbox: std::collections::VecDeque::new(), name: None, protocol: 0, owner: 0, passcred: false }
    }

    fn file() -> Arc<OpenFile> {
        Arc::new(OpenFile { kind: Mutex::new(FileKind::Stdin), flags: Mutex::new(0) })
    }

    fn rights(s: &Socket) -> usize {
        match &s.peer {
            Some(Peer::Pair { channel, side }) => channel.rights_taken(*side).len(),
            _ => 0,
        }
    }

    #[test]
    fn descriptors_arrive_with_the_bytes_they_were_sent_with_in_order() {
        let (peer, mut b) = pair(1, [0; 3], [0; 3]);
        let mut a = end(peer);
        let (x, y) = (file(), file());
        send_with(&mut a, b"one", vec![Arc::clone(&x)]).unwrap();
        send_with(&mut a, b"two", Vec::new()).unwrap();
        send_with(&mut a, b"three", vec![Arc::clone(&y)]).unwrap();
        let mut buf = [0u8; 2];
        assert_eq!(rights(&b), 0, "nothing read yet");
        assert_eq!(receive(&mut b, &mut buf), Ok(2));
        let taken = match &b.peer { Some(Peer::Pair { channel, side }) => channel.rights_taken(*side), _ => Vec::new() };
        assert!(taken.len() == 1 && Arc::ptr_eq(&taken[0], &x), "the first message's, once its first byte is read");
        let mut buf = [0u8; 4];
        assert_eq!(receive(&mut b, &mut buf), Ok(4));
        assert_eq!(rights(&b), 0, "\"e\" and \"two\": none");
        let mut buf = [0u8; 16];
        assert_eq!(receive(&mut b, &mut buf), Ok(5));
        assert_eq!(rights(&b), 1, "the third message's");
    }

    #[test]
    fn a_relay_takes_the_bytes_and_their_descriptors_together() {
        let (peer, b) = pair(1, [0; 3], [0; 3]);
        let mut a = end(peer);
        send_with(&mut a, b"x", vec![file(), file()]).unwrap();
        send_with(&mut a, b"y", vec![file()]).unwrap();
        match take(&b) {
            crate::relay::Took::Data(bytes, rights) => assert_eq!((bytes.as_slice(), rights.len()), (&b"xy"[..], 3)),
            _ => panic!("data"),
        }
    }
}
