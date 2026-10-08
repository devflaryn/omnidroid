//! Internet sockets on the host's network (D30). An `AF_INET`/`AF_INET6` stream or datagram socket
//! (TCP or UDP) is a host socket made through `omni_platform::net` -- the only place a socket call
//! is made -- under the policy the embedding sets ([`set_policy`]; [`NetPolicy::unrestricted`]
//! until it does, as the HLE runtime's game embedding runs). The guest shares the host's port
//! space: a port another host program holds is `EADDRINUSE` here, as it is in a container that
//! uses the host's network. Its addresses are still checked as a machine whose only interface is
//! lo (`crate::inet::check`), which is what `crate::netlink` reports.
//!
//! # Blocking
//!
//! The host socket is always non-blocking. Whether a guest call waits is its descriptor's
//! `O_NONBLOCK` (`socket`/`accept4` flags, `fcntl(F_SETFL)`, `ioctl(FIONBIO)`) and the call's
//! `MSG_DONTWAIT`: a call that would block waits in `crate::poll` as every waiter here does --
//! interrupted by a signal (`EINTR`), bounded by `SO_RCVTIMEO`/`SO_SNDTIMEO` (`EAGAIN`) -- and
//! holds no lock while it waits, so a thread blocked in `recv` stalls nothing else.
//!
//! # Waking a waiter
//!
//! `crate::poll` waits on a change counter, and a host socket becoming readable is not a change
//! anything in this process makes. So one watcher thread per host process waits in the host's
//! readiness call on every host socket for what that socket lacks (readable while it is not,
//! writable while it is not -- never what it already has, so nothing spins on a socket nobody
//! reads) and bumps the counter when any socket's readiness changes. A guest that finds a socket's
//! readiness different from what the watcher last saw -- it drained it, or started a connect --
//! wakes the watcher (a datagram to the watcher's own loopback socket) before it sleeps, so the
//! watcher's set is current. Were the watcher not to start, waiters still see a change within
//! `crate::poll`'s 50 ms slice: slower, never wrong.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use omni_platform::net::{
    self as platnet, ConnectOutcome, ConnectProgress, Interest, IpFamily, NetError, NetErrorKind, NetPolicy,
    OptionValue, PollEntry, Shutdown, SocketAddress, SocketKind, SocketOption, SocketQuery,
};
use parking_lot::{Mutex, RwLock, RwLockWriteGuard};

use crate::errno::{
    Errno, EACCES, EADDRINUSE, EADDRNOTAVAIL, EAFNOSUPPORT, EAGAIN, EALREADY, ECONNABORTED, ECONNREFUSED, ECONNRESET,
    EDESTADDRREQ, EHOSTUNREACH, EINPROGRESS, EINTR, EINVAL, EIO, EISCONN, EMSGSIZE, ENETUNREACH, ENOBUFS,
    ENOPROTOOPT, ENOSYS, ENOTCONN, EOPNOTSUPP, EPERM, EPIPE, ETIMEDOUT,
};
use crate::inet::{AF_INET, AF_INET6};
use crate::poll::{ERR, HUP, IN, OUT};
use crate::process::Task;

pub const MSG_PEEK: u64 = 0x2;
pub const MSG_TRUNC: u64 = 0x20;
pub const MSG_DONTWAIT: u64 = 0x40;
pub const MSG_WAITALL: u64 = 0x100;
pub const MSG_NOSIGNAL: u64 = 0x4000;
const SIGPIPE: u64 = 13;

const SOL_SOCKET: u64 = 1;
const IPPROTO_TCP: u64 = 6;
const IPPROTO_UDP: u64 = 17;
const IPPROTO_IPV6: u64 = 41;

// ------------------------------------------------------------------------------------ policy

static POLICY: RwLock<Option<Arc<NetPolicy>>> = RwLock::new(None);

/// Which network this host process's guests may reach: every host socket made from now on is made
/// under `policy`. Unset, it is [`NetPolicy::unrestricted`] -- the real-AOSP path's guests are the
/// system's own processes and the apps it installs, and the owner's ruling (D30) is that the guest
/// gets a real network.
pub fn set_policy(policy: Arc<NetPolicy>) {
    *POLICY.write() = Some(policy);
}

/// The policy host sockets and the kernel's DNS answers (`crate::dnsproxy`) are made under.
#[must_use]
pub fn policy() -> Arc<NetPolicy> {
    POLICY.read().clone().unwrap_or_else(|| Arc::new(NetPolicy::unrestricted()))
}

/// `OMNI_NET_TRACE=1`: every host socket failure, with the host's own words.
fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_NET_TRACE").as_deref() == Ok("1"))
}

// ------------------------------------------------------------------------------------ errors

/// The Linux errno for a classified host failure. `Other` -- a failure the host's error could not
/// be classified as -- is `EIO`, the kernel's "the lower layer failed".
#[must_use]
pub fn kind_errno(kind: NetErrorKind) -> Errno {
    match kind {
        NetErrorKind::WouldBlock => EAGAIN,
        NetErrorKind::InProgress => EINPROGRESS,
        NetErrorKind::AlreadyConnected => EISCONN,
        NetErrorKind::NotConnected => ENOTCONN,
        NetErrorKind::ConnectionRefused => ECONNREFUSED,
        NetErrorKind::ConnectionReset => ECONNRESET,
        NetErrorKind::ConnectionAborted => ECONNABORTED,
        NetErrorKind::AddressInUse => EADDRINUSE,
        NetErrorKind::AddressNotAvailable => EADDRNOTAVAIL,
        NetErrorKind::NetworkUnreachable => ENETUNREACH,
        NetErrorKind::HostUnreachable => EHOSTUNREACH,
        NetErrorKind::TimedOut => ETIMEDOUT,
        NetErrorKind::BrokenPipe => EPIPE,
        NetErrorKind::PermissionDenied => EACCES,
        NetErrorKind::InvalidInput => EINVAL,
        NetErrorKind::AddressFamilyNotSupported => EAFNOSUPPORT,
        NetErrorKind::MessageSize => EMSGSIZE,
        NetErrorKind::Interrupted => EINTR,
        NetErrorKind::NoBufferSpace => ENOBUFS,
        _ => EIO,
    }
}

/// The errno a guest is told for a failure of the host socket layer. A destination the policy
/// does not admit is `EPERM`, as a device answers a connect its firewall (netd's eBPF) refuses;
/// an option the layer does not implement is `ENOPROTOOPT`; a call it refuses is `EOPNOTSUPP`.
#[must_use]
pub fn errno(error: &NetError) -> Errno {
    let e = match error {
        NetError::Io { kind, .. } => kind_errno(*kind),
        NetError::Policy { .. } => EPERM,
        NetError::UnimplementedOption { .. } => ENOPROTOOPT,
        NetError::Unsupported { .. } => ENOSYS,
        NetError::Refused { .. } => EOPNOTSUPP,
        _ => error.kind().map_or(EIO, kind_errno),
    };
    if trace() && e != EAGAIN {
        eprintln!("[net] {error} -> errno {}", e.0);
    }
    e
}

// ------------------------------------------------------------------------------------ addresses

/// A guest `sockaddr` for a socket of `domain` as the host's address. An `AF_INET` address given
/// to an `AF_INET6` socket is its v4-mapped form, as Linux connects and sends to it.
///
/// # Errors
/// `EINVAL` for a short address, `EAFNOSUPPORT` for a family the socket cannot reach.
pub fn parse(domain: u16, raw: &[u8]) -> Result<SocketAddress, Errno> {
    if raw.len() < 2 {
        return Err(EINVAL);
    }
    match u16::from_le_bytes([raw[0], raw[1]]) {
        AF_INET => {
            if raw.len() < 16 {
                return Err(EINVAL);
            }
            let port = u16::from_be_bytes([raw[2], raw[3]]);
            let a: [u8; 4] = raw[4..8].try_into().expect("4");
            if domain == AF_INET6 {
                let mut address = [0u8; 16];
                address[10] = 0xff;
                address[11] = 0xff;
                address[12..].copy_from_slice(&a);
                return Ok(SocketAddress::V6 { address, port, flowinfo: 0, scope_id: 0 });
            }
            Ok(SocketAddress::V4 { address: a, port })
        }
        AF_INET6 if domain == AF_INET6 => {
            if raw.len() < 24 {
                return Err(EINVAL);
            }
            let port = u16::from_be_bytes([raw[2], raw[3]]);
            // std carries sin6_flowinfo and sin6_scope_id as the bits in memory: no swap.
            let flowinfo = u32::from_le_bytes(raw[4..8].try_into().expect("4"));
            let address: [u8; 16] = raw[8..24].try_into().expect("16");
            let scope_id = raw.get(24..28).map_or(0, |s| u32::from_le_bytes(s.try_into().expect("4")));
            Ok(SocketAddress::V6 { address, port, flowinfo, scope_id })
        }
        _ => Err(EAFNOSUPPORT),
    }
}

/// A host address as the guest's `sockaddr_in` (16 bytes) or `sockaddr_in6` (28).
#[must_use]
pub fn sockaddr(addr: &SocketAddress) -> Vec<u8> {
    match *addr {
        SocketAddress::V4 { address, port } => {
            let mut b = vec![0u8; 16];
            b[0..2].copy_from_slice(&AF_INET.to_le_bytes());
            b[2..4].copy_from_slice(&port.to_be_bytes());
            b[4..8].copy_from_slice(&address);
            b
        }
        SocketAddress::V6 { address, port, flowinfo, scope_id } => {
            let mut b = vec![0u8; 28];
            b[0..2].copy_from_slice(&AF_INET6.to_le_bytes());
            b[2..4].copy_from_slice(&port.to_be_bytes());
            b[4..8].copy_from_slice(&flowinfo.to_le_bytes());
            b[8..24].copy_from_slice(&address);
            b[24..28].copy_from_slice(&scope_id.to_le_bytes());
            b
        }
    }
}

/// `::ffff:0.0.0.0`: the wildcard as an IPv4 address on an IPv6 socket.
fn is_mapped_wildcard(raw: &[u8]) -> bool {
    u16::from_le_bytes([raw.first().copied().unwrap_or(0), raw.get(1).copied().unwrap_or(0)]) == AF_INET6
        && raw.get(8..24).is_some_and(|a| a[..10].iter().all(|b| *b == 0) && a[10] == 0xff && a[11] == 0xff && a[12..].iter().all(|b| *b == 0))
}

/// A destination the host routes to its own loopback: a loopback address, or the wildcard (Linux
/// and macOS connect and send to `0.0.0.0` as to loopback). In a namespace both name the
/// namespace's ports, never the host's.
fn is_local_dest(raw: &[u8]) -> bool {
    crate::loopns::is_loopback(raw) || crate::loopns::is_wildcard(raw) || is_mapped_wildcard(raw)
}

/// The host address a local destination resolves to: the same address with the host's port, a
/// wildcard destination as the loopback of its family (Windows refuses to connect to `0.0.0.0`).
fn loopback_dest(raw: &[u8], port: u16) -> Vec<u8> {
    let mut out = crate::loopns::with_port(raw, port);
    if is_mapped_wildcard(raw) {
        out[20..24].copy_from_slice(&[127, 0, 0, 1]);
    } else if crate::loopns::is_wildcard(raw) {
        if out[0] == 2 {
            out[4..8].copy_from_slice(&[127, 0, 0, 1]);
        } else {
            out[23] = 1; // ::1
        }
    }
    out
}

/// `addr` with another port.
fn with_sa_port(addr: SocketAddress, port: u16) -> SocketAddress {
    match addr {
        SocketAddress::V4 { address, .. } => SocketAddress::V4 { address, port },
        SocketAddress::V6 { address, flowinfo, scope_id, .. } => SocketAddress::V6 { address, port, flowinfo, scope_id },
    }
}

// ------------------------------------------------------------------------------------ sockets

/// Whether `socket(domain, ty, protocol)` is one a host socket stands behind: TCP or UDP over IPv4
/// or IPv6. Raw and ICMP sockets keep the loopback-only kernel's answers (`crate::inet`).
#[must_use]
pub fn eligible(domain: u64, ty: u64, protocol: u64) -> bool {
    matches!(domain, 2 | 10) && matches!((ty, protocol), (1, 0 | IPPROTO_TCP) | (2, 0 | IPPROTO_UDP))
}

#[derive(Default)]
struct State {
    bound: bool,
    connecting: bool,
    /// A non-blocking connect to a loopback port the namespace does not hold: Linux answers such
    /// a connect `EINPROGRESS` and then reports the refusal (readable, writable, in error, hung
    /// up; `SO_ERROR` once), so the socket carries it as a failed connect the host never saw.
    refused: bool,
    connected: bool,
    listening: bool,
    shut_rd: bool,
    shut_wr: bool,
    /// `SO_REUSEADDR` as the guest set it. A stream socket's is kept here and not given to the
    /// host: Winsock's `SO_REUSEADDR` lets a second socket take a port another is listening on,
    /// which Linux's never does. The cost, on a Linux or macOS host only: a guest server restarted
    /// while its old connections are in TIME_WAIT is refused its port (`EADDRINUSE`) until they
    /// expire. A datagram socket's is the host's (a shared port: mDNS's 5353).
    reuse: bool,
    rcvtimeo: Option<Duration>,
    sndtimeo: Option<Duration>,
}

/// One guest internet socket's host socket.
pub struct Host {
    sock: RwLock<platnet::Socket>,
    /// `AF_INET` or `AF_INET6`.
    pub domain: u16,
    /// `SOCK_STREAM` (TCP); otherwise `SOCK_DGRAM` (UDP).
    pub stream: bool,
    /// The readiness the watcher last saw (`POLL*` bits), `UNSEEN` before it has looked.
    seen: AtomicU32,
    state: Mutex<State>,
    /// The loopback namespace of this socket's loopback and wildcard ports (`crate::loopns`);
    /// `None` for a process without an instance: the host's loopback, as before.
    ns: Option<Arc<crate::loopns::Namespace>>,
    /// The namespace entries this socket holds; released when it is dropped.
    held: Mutex<Vec<crate::loopns::Binding>>,
    /// The port the guest bound, where it is not the host's (`getsockname`).
    guest_port: Mutex<Option<u16>>,
    /// The loopback address the guest connected to, as it named it (`getpeername`).
    guest_peer: Mutex<Option<Vec<u8>>>,
    /// The 64 KiB a namespace-mode datagram receive takes a datagram into, sized on first use and
    /// reused (held only for a receive and its copy), so the receive path -- every poll of a game's
    /// socket -- allocates nothing.
    scratch: Mutex<Vec<u8>>,
}

const UNSEEN: u32 = u32::MAX;

enum Settled {
    Pending,
    Connected,
    Failed(Errno),
}

impl Host {
    /// `socket(domain, SOCK_STREAM | SOCK_DGRAM, ...)`: a host socket under [`policy`]. An IPv6
    /// socket is dual-stack, as Linux's default (`bindv6only` 0) makes it -- Android's Java stack
    /// connects to IPv4 servers through v4-mapped addresses on IPv6 sockets -- where Windows'
    /// default is v6-only.
    ///
    /// # Errors
    /// The host's refusal, as an errno.
    pub fn create(domain: u16, stream: bool, ns: Option<Arc<crate::loopns::Namespace>>) -> Result<Arc<Self>, Errno> {
        let sock = Self::fresh(domain, stream)?;
        Ok(Self::adopt(sock, domain, stream, State::default(), ns))
    }

    /// A new unbound, non-blocking host socket (dual-stack for IPv6).
    fn fresh(domain: u16, stream: bool) -> Result<platnet::Socket, Errno> {
        let family = if domain == AF_INET { IpFamily::V4 } else { IpFamily::V6 };
        let kind = if stream { SocketKind::Stream } else { SocketKind::Datagram };
        let mut sock = platnet::Socket::new(kind, family, policy()).map_err(|e| errno(&e))?;
        sock.set_nonblocking(true).map_err(|e| errno(&e))?;
        if family == IpFamily::V6 {
            sock.set_option(SocketOption::V6Only(false)).map_err(|e| errno(&e))?;
        }
        Ok(sock)
    }

    fn adopt(sock: platnet::Socket, domain: u16, stream: bool, state: State, ns: Option<Arc<crate::loopns::Namespace>>) -> Arc<Self> {
        let host = Arc::new(Self {
            sock: RwLock::new(sock),
            domain,
            stream,
            seen: AtomicU32::new(UNSEEN),
            state: Mutex::new(state),
            ns,
            held: Mutex::new(Vec::new()),
            guest_port: Mutex::new(None),
            guest_peer: Mutex::new(None),
            scratch: Mutex::new(Vec::new()),
        });
        watch(&host);
        host
    }

    fn proto(&self) -> crate::loopns::Proto {
        if self.stream { crate::loopns::Proto::Tcp } else { crate::loopns::Proto::Udp }
    }

    /// The host's port for this socket.
    fn host_port(&self) -> Result<u16, Errno> {
        let addr = self.sock.read().local_address().map_err(|e| errno(&e))?;
        Ok(match addr {
            SocketAddress::V4 { port, .. } | SocketAddress::V6 { port, .. } => port,
        })
    }

    /// The host socket, to change: the watcher holds it shared while it waits, so it is asked to
    /// let go first.
    fn sock_mut(&self) -> RwLockWriteGuard<'_, platnet::Socket> {
        if let Some(g) = self.sock.try_write() {
            return g;
        }
        wake();
        self.sock.write()
    }

    /// `bind(addr)`.
    ///
    /// # Errors
    /// `EINVAL` when already bound or the address is short, `EAFNOSUPPORT`, `EADDRNOTAVAIL` for an
    /// address lo does not have, and the host's refusal (`EADDRINUSE`).
    pub fn bind(&self, raw: &[u8]) -> Result<(), Errno> {
        crate::inet::check(self.domain, raw)?;
        if self.state.lock().bound {
            return Err(EINVAL);
        }
        let Some(ns) = self.ns.clone() else {
            let addr = parse(self.domain, raw)?;
            self.sock_mut().bind(&addr).map_err(|e| errno(&e))?;
            self.state.lock().bound = true;
            wake();
            return Ok(());
        };
        // `inet::check` let through only loopback and the wildcard: bound on the host as the same
        // address with port 0, and the guest's port is the namespace's.
        let guest = crate::loopns::port_of(raw);
        let host_addr = parse(self.domain, &crate::loopns::with_port(raw, 0))?;
        self.sock_mut().bind(&host_addr).map_err(|e| errno(&e))?;
        let host = self.host_port()?;
        let shared = self.state.lock().reuse && !self.stream;
        match ns.bind(self.proto(), guest, host, shared) {
            Ok(b) => self.held.lock().push(b),
            Err(e) => {
                // The guest's bind failed: its socket is unbound again.
                *self.sock_mut() = Self::fresh(self.domain, self.stream)?;
                return Err(e);
            }
        }
        *self.guest_port.lock() = Some(if guest == 0 { host } else { guest });
        self.state.lock().bound = true;
        wake();
        Ok(())
    }

    /// For a loopback destination: the host address it resolves to in this socket's namespace
    /// (the same address, the host's port), binding an unbound socket first -- the same loopback
    /// address for a stream, the wildcard for a datagram -- so its peer can resolve it too.
    /// `Ok(None)`: not a loopback destination, or no namespace: as given. `Err(ECONNREFUSED)`: a
    /// port no socket of the namespace holds.
    fn resolve(&self, raw: &[u8]) -> Result<Option<Vec<u8>>, Errno> {
        let Some(ns) = &self.ns else { return Ok(None) };
        if !is_local_dest(raw) {
            return Ok(None);
        }
        let host = ns.lookup(self.proto(), crate::loopns::port_of(raw)).ok_or(ECONNREFUSED)?;
        if !self.state.lock().bound {
            let local = if self.stream && crate::loopns::is_loopback(raw) { crate::loopns::with_port(raw, 0) } else { crate::loopns::wildcard(self.domain) };
            self.bind(&local)?;
        }
        Ok(Some(loopback_dest(raw, host)))
    }

    /// The guest closed its last descriptor: its ports in the namespace are free at once. The host
    /// socket itself is closed when the watcher lets go of it (its port is the host's, never the
    /// guest's), which may be a moment later.
    pub fn release(&self) {
        self.held.lock().clear();
    }

    /// A refused connect's pending error, taken: reported once, as Linux's `sk_err` is.
    fn take_refused(&self) -> bool {
        std::mem::take(&mut self.state.lock().refused)
    }

    /// The host socket replaced by a fresh unbound one and the namespace entries it held
    /// released: the guest's socket as it was before its bind (or its host-side connect).
    fn unbind(&self) -> Result<(), Errno> {
        *self.sock_mut() = Self::fresh(self.domain, self.stream)?;
        self.held.lock().clear();
        *self.guest_port.lock() = None;
        let mut st = self.state.lock();
        st.bound = false;
        st.connected = false;
        st.connecting = false;
        Ok(())
    }

    /// `connect(addr)`: a datagram socket's default peer; a stream's connection, waited for unless
    /// `nonblocking` (then `EINPROGRESS`, and `SO_ERROR` says how it went).
    ///
    /// # Errors
    /// `EISCONN`, `EALREADY`, `EINPROGRESS`, `EINTR` (the connection goes on), and the connection's
    /// own failure (`ECONNREFUSED`, `ETIMEDOUT`, `ENETUNREACH`, ...).
    pub fn connect(&self, raw: &[u8], nonblocking: bool, t: &Task) -> Result<(), Errno> {
        if !self.stream {
            // A datagram socket's connect replaces the peer it named before.
            let was_named = self.guest_peer.lock().take().is_some();
            if self.ns.is_some() {
                let local = is_local_dest(raw);
                if local && self.state.lock().connected && !was_named {
                    // Host-connected to another peer: the host socket cannot be un-connected.
                    self.unbind()?;
                }
                if !self.state.lock().bound {
                    self.bind(&crate::loopns::wildcard(self.domain))?;
                }
                if local {
                    // Connecting a datagram socket only names its peer (Linux): resolved at each
                    // send, so a send before the peer binds is dropped and a peer that binds later
                    // is reached.
                    *self.guest_peer.lock() = Some(raw.to_vec());
                    let mut st = self.state.lock();
                    st.connected = true;
                    st.bound = true;
                    return Ok(());
                }
                if was_named {
                    self.state.lock().connected = false;
                }
            }
        }
        if self.stream && self.take_refused() {
            return Err(ECONNREFUSED);
        }
        let translated = match self.resolve(raw) {
            Err(e) if e == ECONNREFUSED && nonblocking && self.stream => {
                self.state.lock().refused = true;
                crate::poll::notify_key(self.key());
                wake();
                return Err(EINPROGRESS);
            }
            other => other?,
        };
        if translated.is_some() {
            *self.guest_peer.lock() = Some(raw.to_vec());
        }
        let raw: &[u8] = translated.as_deref().unwrap_or(raw);
        let addr = parse(self.domain, raw)?;
        if !self.stream {
            self.sock_mut().connect(&addr).map_err(|e| errno(&e))?;
            let mut st = self.state.lock();
            st.connected = true;
            st.bound = true;
            return Ok(());
        }
        let (connected, connecting, listening, timeout) = {
            let st = self.state.lock();
            (st.connected, st.connecting, st.listening, st.sndtimeo)
        };
        if connected || listening {
            return Err(EISCONN);
        }
        if connecting {
            return match self.settle()? {
                Settled::Connected => Err(EISCONN),
                Settled::Failed(e) => Err(e),
                Settled::Pending if nonblocking => Err(EALREADY),
                Settled::Pending => self.wait_connected(timeout, t),
            };
        }
        let progress = self.sock_mut().connect(&addr).map_err(|e| errno(&e))?;
        {
            let mut st = self.state.lock();
            st.bound = true;
            match progress {
                ConnectProgress::Connected => st.connected = true,
                ConnectProgress::InProgress => st.connecting = true,
            }
        }
        wake();
        match progress {
            ConnectProgress::Connected => Ok(()),
            ConnectProgress::InProgress if nonblocking => Err(EINPROGRESS),
            ConnectProgress::InProgress => self.wait_connected(timeout, t),
        }
    }

    /// A blocking connect's wait: the connection, its failure, `SO_SNDTIMEO` running out
    /// (`EINPROGRESS`, as Linux answers), or a signal (`EINTR`).
    fn wait_connected(&self, timeout: Option<Duration>, t: &Task) -> Result<(), Errno> {
        let deadline = timeout.and_then(|d| Instant::now().checked_add(d));
        loop {
            let seen = crate::poll::generation();
            match self.settle()? {
                Settled::Connected => return Ok(()),
                Settled::Failed(e) => {
                    // Reported here, so not again by SO_ERROR (Linux's `sock_error` takes it).
                    let _ = self.sock_mut().get_option(SocketQuery::Error);
                    return Err(e);
                }
                Settled::Pending => {}
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(EINPROGRESS);
            }
            let _ = self.readiness();
            crate::poll::wait_for_change(seen, deadline, t)?;
        }
    }

    /// How a started connect stands. The failure stays the socket's pending error until
    /// `SO_ERROR` is read.
    fn settle(&self) -> Result<Settled, Errno> {
        if !self.state.lock().connecting {
            return Ok(if self.state.lock().connected { Settled::Connected } else { Settled::Pending });
        }
        let outcome = self.sock_mut().connect_result().map_err(|e| errno(&e))?;
        let settled = match outcome {
            ConnectOutcome::NotStarted | ConnectOutcome::InProgress => return Ok(Settled::Pending),
            ConnectOutcome::Connected => Settled::Connected,
            ConnectOutcome::Failed(kind) => Settled::Failed(kind_errno(kind)),
        };
        let mut st = self.state.lock();
        st.connecting = false;
        st.connected = matches!(settled, Settled::Connected);
        drop(st);
        wake();
        Ok(settled)
    }

    /// `listen(backlog)`: a stream socket takes connections (bound to the wildcard address and an
    /// ephemeral port first when nothing bound it, as Linux binds it).
    ///
    /// # Errors
    /// `EOPNOTSUPP` on a datagram socket, `EINVAL` on a connected one, the host's refusal.
    pub fn listen(&self, backlog: i32) -> Result<(), Errno> {
        if !self.stream {
            return Err(EOPNOTSUPP);
        }
        {
            let st = self.state.lock();
            if st.connected || st.connecting {
                return Err(EINVAL);
            }
        }
        if self.ns.is_some() && !self.state.lock().bound {
            self.bind(&crate::loopns::wildcard(self.domain))?;
        }
        self.sock_mut().listen(backlog.clamp(0, 4096)).map_err(|e| errno(&e))?;
        let mut st = self.state.lock();
        st.listening = true;
        st.bound = true;
        st.shut_rd = false;
        drop(st);
        wake();
        Ok(())
    }

    /// `accept`: the next connection and its peer's address, waited for unless `nonblocking`.
    ///
    /// # Errors
    /// `EINVAL` when not listening (or shut down for reading), `EOPNOTSUPP` on a datagram socket,
    /// `EAGAIN`, `EINTR`.
    pub fn accept(&self, nonblocking: bool, t: &Task) -> Result<(Arc<Self>, Vec<u8>), Errno> {
        if !self.stream {
            return Err(EOPNOTSUPP);
        }
        let deadline = self.deadline(false);
        loop {
            let (sock, peer) = self.retry(IN, nonblocking, deadline, t, || {
                let st = self.state.lock();
                if !st.listening || st.shut_rd {
                    return Err(EINVAL);
                }
                drop(st);
                self.sock.read().accept().map_err(|e| errno(&e))
            })?;
            let mut raw_peer = sockaddr(&peer);
            if let Some(ns) = &self.ns {
                // Only a connection from this namespace: loopback, from a port of its own.
                let theirs = crate::loopns::is_loopback(&raw_peer)
                    .then(|| ns.guest_port(crate::loopns::Proto::Tcp, crate::loopns::port_of(&raw_peer)))
                    .flatten();
                match theirs {
                    Some(g) => raw_peer = crate::loopns::with_port(&raw_peer, g),
                    None => {
                        drop(sock); // closed: another instance's, or a host program's
                        continue;
                    }
                }
            }
            let mut sock = sock;
            sock.set_nonblocking(true).map_err(|e| errno(&e))?;
            let state = State { bound: true, connected: true, ..State::default() };
            let accepted = Self::adopt(sock, self.domain, true, state, self.ns.clone());
            *accepted.guest_port.lock() = *self.guest_port.lock();
            return Ok((accepted, raw_peer));
        }
    }

    fn deadline(&self, send: bool) -> Option<Instant> {
        let st = self.state.lock();
        let timeout = if send { st.sndtimeo } else { st.rcvtimeo };
        timeout.and_then(|d| Instant::now().checked_add(d))
    }

    /// Run `op` until it does not answer `EAGAIN`: once when `nonblocking`, else waiting for the
    /// readiness `want` between tries, until `deadline` (then `EAGAIN`) or a signal (`EINTR`).
    fn retry<T>(&self, want: u32, nonblocking: bool, deadline: Option<Instant>, t: &Task, mut op: impl FnMut() -> Result<T, Errno>) -> Result<T, Errno> {
        loop {
            // `poll_keyed`: this socket's changes only (the watcher tells each socket's by its key),
            // not every change in the process.
            let keyed = (!nonblocking && crate::poll::KEYED.load(Ordering::Relaxed)).then(|| crate::poll::watch(Some(vec![self.key()])));
            let seen = crate::poll::generation();
            match op() {
                Err(e) if e == EAGAIN => {}
                other => return other,
            }
            if nonblocking {
                return Err(EAGAIN);
            }
            let now = Instant::now();
            if deadline.is_some_and(|d| now >= d) {
                return Err(EAGAIN);
            }
            // Ready and still refused (a datagram the host dropped between the two): try again
            // shortly rather than wait for a change that has already happened.
            let wake_at = if self.readiness() & (want | ERR | HUP) != 0 {
                let soon = now + Duration::from_millis(1);
                Some(deadline.map_or(soon, |d| d.min(soon)))
            } else {
                deadline
            };
            match &keyed {
                Some(watch) => watch.wait(wake_at, t)?,
                None => crate::poll::wait_for_change(seen, wake_at, t)?,
            }
        }
    }

    /// What this socket's changes are told by (`crate::socket::key` gives a guest's poll the same).
    pub(crate) fn key(&self) -> crate::poll::Key {
        std::ptr::from_ref(self) as crate::poll::Key
    }

    /// Whether a stream's connect is still going: settled first, so a send or receive on a socket
    /// whose connect finished just works.
    fn still_connecting(&self) -> Result<bool, Errno> {
        if !self.state.lock().connecting {
            return Ok(false);
        }
        match self.settle()? {
            Settled::Pending => Ok(true),
            Settled::Connected => Ok(false),
            Settled::Failed(e) => {
                let _ = self.sock_mut().get_option(SocketQuery::Error);
                Err(e)
            }
        }
    }

    /// `send`/`sendto`/`sendmsg`/`write`: `to` is a datagram's destination (a stream ignores it,
    /// as Linux's TCP does). A blocking stream send sends all of `bytes` unless interrupted after
    /// some went; `EPIPE` raises `SIGPIPE` unless `MSG_NOSIGNAL`.
    ///
    /// # Errors
    /// `EPIPE` (shut down, or a stream never connected), `EDESTADDRREQ`, `EAGAIN`, `EINTR`,
    /// `ECONNRESET`, `EMSGSIZE`, ...
    pub fn send(&self, bytes: &[u8], to: Option<&[u8]>, flags: u64, nonblocking: bool, t: &Task) -> Result<usize, Errno> {
        let r = self.send_inner(bytes, to, flags, nonblocking || flags & MSG_DONTWAIT != 0, t);
        if r == Err(EPIPE) && flags & MSG_NOSIGNAL == 0 {
            t.pending.fetch_or(1 << (SIGPIPE - 1), Ordering::SeqCst);
        }
        r
    }

    fn send_inner(&self, bytes: &[u8], to: Option<&[u8]>, _flags: u64, nonblocking: bool, t: &Task) -> Result<usize, Errno> {
        if self.stream && self.take_refused() {
            return Err(ECONNREFUSED);
        }
        if self.ns.is_some() && !self.stream && !self.state.lock().bound {
            self.bind(&crate::loopns::wildcard(self.domain))?;
        }
        let named_peer = if self.stream { None } else { self.guest_peer.lock().clone() };
        let to = to.or(named_peer.as_deref());
        let dest = match to {
            Some(raw) if !self.stream => match self.resolve(raw) {
                Ok(Some(host)) => Some(parse(self.domain, &host)?),
                Ok(None) => Some(parse(self.domain, raw)?),
                // A datagram to a port no one here holds: gone, as to a closed port.
                Err(e) if e == ECONNREFUSED => return Ok(bytes.len()),
                Err(e) => return Err(e),
            },
            _ => None,
        };
        {
            let st = self.state.lock();
            if st.shut_wr {
                return Err(EPIPE);
            }
            if self.stream && !st.connected && !st.connecting {
                return Err(EPIPE);
            }
            if !self.stream && dest.is_none() && !st.connected {
                return Err(EDESTADDRREQ);
            }
        }
        let deadline = self.deadline(true);
        let mut done = 0;
        loop {
            let r = self.retry(OUT, nonblocking, deadline, t, || {
                if self.stream && self.still_connecting()? {
                    return Err(EAGAIN);
                }
                let s = self.sock.read();
                let rest = &bytes[done..];
                match &dest {
                    Some(a) => s.send_to(rest, a),
                    None => s.send(rest),
                }
                .map_err(|e| errno(&e))
            });
            match r {
                Ok(n) => {
                    done += n;
                    if !self.stream || nonblocking || done >= bytes.len() || n == 0 {
                        break;
                    }
                }
                Err(_) if done > 0 => break,
                Err(e) => return Err(e),
            }
        }
        if !self.stream {
            self.state.lock().bound = true;
        }
        Ok(done)
    }

    /// `recv`/`recvfrom`/`recvmsg`/`read`: bytes into `buf` and, for a datagram, where they came
    /// from. `MSG_PEEK` leaves them queued; `MSG_WAITALL` on a blocking stream waits for all of
    /// `buf`, end of file or an error after some arrived.
    ///
    /// # Errors
    /// `ENOTCONN` (a stream not connected), `EAGAIN`, `EINTR`, `ECONNRESET`, `ECONNREFUSED` (a
    /// connected datagram socket's peer port is closed).
    pub fn recv(&self, buf: &mut [u8], flags: u64, nonblocking: bool, t: &Task) -> Result<(usize, Option<SocketAddress>), Errno> {
        let nonblocking = nonblocking || flags & MSG_DONTWAIT != 0;
        if self.stream && self.take_refused() {
            return Err(ECONNREFUSED);
        }
        {
            let st = self.state.lock();
            if st.shut_rd {
                return Ok((0, None));
            }
            if self.stream && !st.connected && !st.connecting {
                return Err(ENOTCONN);
            }
        }
        if !self.stream && !self.state.lock().bound {
            // Linux waits on an unbound datagram socket; Winsock refuses a receive there. Bound to
            // the wildcard address and an ephemeral port, it waits on both.
            if self.ns.is_some() {
                self.bind(&crate::loopns::wildcard(self.domain))?;
            } else {
                let any = if self.domain == AF_INET { SocketAddress::unspecified(IpFamily::V4) } else { SocketAddress::unspecified(IpFamily::V6) };
                self.sock_mut().bind(&any).map_err(|e| errno(&e))?;
                self.state.lock().bound = true;
            }
        }
        let deadline = self.deadline(false);
        let peek = flags & MSG_PEEK != 0;
        let waitall = self.stream && !nonblocking && !peek && flags & MSG_WAITALL != 0;
        if let (Some(ns), false) = (&self.ns, self.stream) {
            return self.recv_datagram(ns, buf, peek, nonblocking, deadline, t);
        }
        let mut done = 0;
        let mut from = None;
        loop {
            let r = self.retry(IN, nonblocking, deadline, t, || {
                if self.stream && self.still_connecting()? {
                    return Err(EAGAIN);
                }
                let s = self.sock.read();
                let into = &mut buf[done..];
                let r = if peek {
                    s.peek(into)
                } else if self.stream {
                    s.recv(into).map(|n| (n, None))
                } else {
                    s.recv_from(into).map(|(n, a)| (n, Some(a)))
                };
                match r {
                    Ok(v) => Ok(v),
                    // Winsock reports an ICMP port-unreachable for an earlier datagram on the next
                    // receive; Linux reports it only on a connected socket, as ECONNREFUSED.
                    Err(e) if !self.stream && e.kind() == Some(NetErrorKind::ConnectionReset) => {
                        if self.state.lock().connected { Err(ECONNREFUSED) } else { Err(EAGAIN) }
                    }
                    // A datagram larger than the buffer: what fit, the rest discarded.
                    Err(e) if !self.stream && e.kind() == Some(NetErrorKind::MessageSize) => Ok((into.len(), None)),
                    Err(e) => Err(errno(&e)),
                }
            });
            match r {
                Ok((n, a)) => {
                    done += n;
                    from = from.or(a);
                    if !waitall || n == 0 || done >= buf.len() {
                        break;
                    }
                }
                Err(_) if done > 0 => break,
                Err(e) => return Err(e),
            }
        }
        Ok((done, from))
    }

    /// A datagram receive in a namespace: the datagram is taken whole (64 KiB, the most UDP
    /// carries) so its source is always known -- an oversized one included, which Winsock would
    /// otherwise refuse with no source -- and only what the namespace lets through is passed on:
    /// from a loopback port of the namespace (shown as its guest port), or from a non-loopback
    /// address; and, on a socket connected to a loopback peer, only from that peer. What does
    /// not pass is dropped and the wait goes on. The guest's buffer takes what fits (Linux).
    fn recv_datagram(&self, ns: &crate::loopns::Namespace, buf: &mut [u8], peek: bool, nonblocking: bool, deadline: Option<Instant>, t: &Task) -> Result<(usize, Option<SocketAddress>), Errno> {
        loop {
            let (n, src) = self.retry(IN, nonblocking, deadline, t, || {
                let mut scratch = self.scratch.lock();
                if scratch.len() < 64 << 10 {
                    scratch.resize(64 << 10, 0);
                }
                let s = self.sock.read();
                let r = if peek { s.peek(&mut scratch) } else { s.recv_from(&mut scratch).map(|(n, a)| (n, Some(a))) };
                match r {
                    // The guest's buffer takes what fits, copied before the scratch is let go; it is
                    // only passed on if the datagram then clears the namespace's filter.
                    Ok(v) => {
                        let k = v.0.min(buf.len());
                        buf[..k].copy_from_slice(&scratch[..k]);
                        Ok(v)
                    }
                    // As in `recv`: Winsock's late ICMP port-unreachable.
                    Err(e) if e.kind() == Some(NetErrorKind::ConnectionReset) => {
                        if self.state.lock().connected { Err(ECONNREFUSED) } else { Err(EAGAIN) }
                    }
                    Err(e) => Err(errno(&e)),
                }
            })?;
            let peer = self.guest_peer.lock().as_deref().map(crate::loopns::port_of);
            let shown = match src {
                Some(src) => {
                    let raw_src = sockaddr(&self.guest_family(src));
                    if crate::loopns::is_loopback(&raw_src) {
                        match ns.guest_port(crate::loopns::Proto::Udp, crate::loopns::port_of(&raw_src)) {
                            Some(g) if peer.is_none_or(|p| p == g) => Some(with_sa_port(src, g)),
                            _ => None,
                        }
                    } else if peer.is_none() {
                        Some(src)
                    } else {
                        None
                    }
                }
                None => None,
            };
            match shown {
                Some(from) => {
                    return Ok((n.min(buf.len()), Some(from)));
                }
                None => {
                    // Dropped. A peek saw it without taking it: taken now.
                    if peek {
                        // Locks in the receive's order: scratch, then the socket.
                        let mut scratch = self.scratch.lock();
                        let _ = self.sock.read().recv_from(&mut scratch);
                    }
                }
            }
        }
    }

    /// A send that does not wait and raises no signal: what a descriptor write under its lock
    /// (`sendfile`) does.
    ///
    /// # Errors
    /// As [`send`](Self::send), `EAGAIN` for a send that would wait.
    pub fn try_send(&self, bytes: &[u8]) -> Result<usize, Errno> {
        if self.stream && self.take_refused() {
            return Err(ECONNREFUSED);
        }
        if self.stream && self.still_connecting()? {
            return Err(EAGAIN);
        }
        let st = self.state.lock();
        if st.shut_wr || (self.stream && !st.connected) {
            return Err(EPIPE);
        }
        if !self.stream && !st.connected {
            return Err(EDESTADDRREQ);
        }
        drop(st);
        self.sock.read().send(bytes).map_err(|e| errno(&e))
    }

    /// A receive that does not wait (a stream's bytes, or a datagram's).
    ///
    /// # Errors
    /// As [`recv`](Self::recv), `EAGAIN` when nothing has arrived.
    pub fn try_recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if self.stream && self.take_refused() {
            return Err(ECONNREFUSED);
        }
        let st = self.state.lock();
        if st.shut_rd {
            return Ok(0);
        }
        if self.stream && !st.connected {
            return Err(if st.connecting { EAGAIN } else { ENOTCONN });
        }
        drop(st);
        self.sock.read().recv(buf).map_err(|e| errno(&e))
    }

    /// Bytes ready to read (`FIONREAD`): a stream's queued bytes (as many as one 64 KiB look sees),
    /// a datagram socket's next datagram's size.
    #[must_use]
    pub fn available(&self) -> usize {
        let mut buf = vec![0u8; 64 << 10];
        self.sock.read().peek(&mut buf).map_or(0, |(n, _)| n)
    }

    /// `shutdown(how)`.
    ///
    /// # Errors
    /// `EINVAL` for another `how`, `ENOTCONN` when there is nothing to shut down.
    pub fn shutdown(&self, how: u64) -> Result<(), Errno> {
        let (rd, wr, which) = match how {
            0 => (true, false, Shutdown::Read),
            1 => (false, true, Shutdown::Write),
            2 => (true, true, Shutdown::Both),
            _ => return Err(EINVAL),
        };
        let (connected, connecting, listening) = {
            let st = self.state.lock();
            (st.connected, st.connecting, st.listening)
        };
        if listening {
            // A listener shut down for reading stops taking connections: a thread blocked in
            // `accept` returns EINVAL, which is how servers stop their accept threads.
            if rd {
                self.state.lock().shut_rd = true;
                crate::poll::notify_key(self.key());
            }
            return Ok(());
        }
        if !connected && !connecting {
            return Err(ENOTCONN);
        }
        if self.stream {
            self.sock.read().shutdown(which).map_err(|e| errno(&e))?;
        }
        let mut st = self.state.lock();
        st.shut_rd |= rd;
        st.shut_wr |= wr;
        drop(st);
        crate::poll::notify_key(self.key());
        wake();
        Ok(())
    }

    /// `getsockname`: the host's report, with the port the host gave.
    ///
    /// # Errors
    /// The host's refusal.
    pub fn name(&self) -> Result<Vec<u8>, Errno> {
        let addr = self.sock.read().local_address().map_err(|e| errno(&e))?;
        let raw = sockaddr(&self.guest_family(addr));
        Ok(match *self.guest_port.lock() {
            Some(p) => crate::loopns::with_port(&raw, p),
            None => raw,
        })
    }

    /// `getpeername`.
    ///
    /// # Errors
    /// `ENOTCONN` when there is no peer.
    pub fn peer(&self) -> Result<Vec<u8>, Errno> {
        if self.stream && self.still_connecting()? {
            return Err(ENOTCONN);
        }
        if !self.state.lock().connected {
            return Err(ENOTCONN);
        }
        if let Some(named) = self.guest_peer.lock().clone() {
            return Ok(named);
        }
        let addr = self.sock.read().peer_address().map_err(|e| errno(&e))?;
        let raw = sockaddr(&self.guest_family(addr));
        if let Some(ns) = &self.ns {
            if crate::loopns::is_loopback(&raw) {
                if let Some(g) = ns.guest_port(self.proto(), crate::loopns::port_of(&raw)) {
                    return Ok(crate::loopns::with_port(&raw, g));
                }
            }
        }
        Ok(raw)
    }

    /// A host address in the socket's own family (the host may report an IPv6 socket's unbound
    /// name as the IPv4 wildcard).
    fn guest_family(&self, addr: SocketAddress) -> SocketAddress {
        match (self.domain, addr) {
            (AF_INET6, SocketAddress::V4 { address, port }) => {
                if address == [0; 4] {
                    return SocketAddress::V6 { address: [0; 16], port, flowinfo: 0, scope_id: 0 };
                }
                let mut a = [0u8; 16];
                a[10] = 0xff;
                a[11] = 0xff;
                a[12..].copy_from_slice(&address);
                SocketAddress::V6 { address: a, port, flowinfo: 0, scope_id: 0 }
            }
            (_, a) => a,
        }
    }

    /// `getsockopt(level, name)`: the option's bytes, or `None` for one this socket leaves to the
    /// generic answer (`crate::socket`).
    ///
    /// # Errors
    /// `ENOPROTOOPT` for a TCP option on a datagram socket or an IPv6 one on an IPv4 socket, and
    /// the host's refusal.
    pub fn get_option(&self, level: u64, name: u64) -> Result<Option<Vec<u8>>, Errno> {
        let int = |v: i32| Ok(Some(v.to_le_bytes().to_vec()));
        let flag = |q: SocketQuery| -> Result<Option<Vec<u8>>, Errno> {
            match self.sock_mut().get_option(q).map_err(|e| errno(&e))? {
                OptionValue::Flag(on) => int(i32::from(on)),
                OptionValue::Bytes(n) => int(i32::try_from(n).unwrap_or(i32::MAX)),
                OptionValue::Interval(d) => int(i32::try_from(d.as_secs()).unwrap_or(i32::MAX)),
                OptionValue::Count(n) => int(i32::try_from(n).unwrap_or(i32::MAX)),
                _ => Ok(None),
            }
        };
        match (level, name) {
            (SOL_SOCKET, 4) => {
                // SO_ERROR: a connect that has settled is settled first, so its failure is here.
                if self.take_refused() {
                    return int(ECONNREFUSED.0);
                }
                if self.stream && self.state.lock().connecting {
                    let _ = self.settle()?;
                }
                match self.sock_mut().get_option(SocketQuery::Error).map_err(|e| errno(&e))? {
                    OptionValue::Error(Some(kind)) => int(kind_errno(kind).0),
                    _ => int(0),
                }
            }
            (SOL_SOCKET, 2) => int(i32::from(self.state.lock().reuse)),
            (SOL_SOCKET, 6) => flag(SocketQuery::Broadcast),
            (SOL_SOCKET, 9) => flag(SocketQuery::KeepAlive),
            (SOL_SOCKET, 7) => flag(SocketQuery::SendBuffer),
            (SOL_SOCKET, 8) => flag(SocketQuery::ReceiveBuffer),
            (SOL_SOCKET, 20 | 21 | 66 | 67) => {
                let st = self.state.lock();
                let d = if matches!(name, 20 | 66) { st.rcvtimeo } else { st.sndtimeo }.unwrap_or_default();
                let mut tv = d.as_secs().to_le_bytes().to_vec();
                tv.extend_from_slice(&u64::from(d.subsec_micros()).to_le_bytes());
                Ok(Some(tv))
            }
            (SOL_SOCKET, 13) => match self.sock_mut().get_option(SocketQuery::Linger).map_err(|e| errno(&e))? {
                OptionValue::Linger(l) => {
                    let mut b = i32::from(l.is_some()).to_le_bytes().to_vec();
                    b.extend_from_slice(&(l.map_or(0, |d| i32::try_from(d.as_secs()).unwrap_or(i32::MAX))).to_le_bytes());
                    Ok(Some(b))
                }
                _ => Ok(None),
            },
            (SOL_SOCKET, 30) => int(i32::from(self.state.lock().listening)), // SO_ACCEPTCONN
            (SOL_SOCKET, 38) => int(if self.stream { 6 } else { 17 }),      // SO_PROTOCOL
            (SOL_SOCKET, 39) => int(i32::from(self.domain)),                // SO_DOMAIN
            (IPPROTO_TCP, _) if !self.stream => Err(ENOPROTOOPT),
            (IPPROTO_TCP, 1) => flag(SocketQuery::NoDelay),
            (IPPROTO_TCP, 4) => flag(SocketQuery::KeepAliveIdle),
            (IPPROTO_TCP, 5) => flag(SocketQuery::KeepAliveInterval),
            (IPPROTO_TCP, 6) => flag(SocketQuery::KeepAliveCount),
            (IPPROTO_IPV6, _) if self.domain != AF_INET6 => Err(ENOPROTOOPT),
            (IPPROTO_IPV6, 26) => flag(SocketQuery::V6Only),
            _ => Ok(None),
        }
    }

    /// `setsockopt(level, name, value)`: `Ok(true)` when this socket acted on it, `Ok(false)` for
    /// an option left to the generic answer (accepted, as it was before sockets reached the host:
    /// multicast membership, `IP_TOS`, `SO_MARK` -- netd's fwmark, which the host has no use for).
    ///
    /// # Errors
    /// `EINVAL` for a short value, `ENOPROTOOPT` as [`get_option`](Self::get_option), and the
    /// host's refusal.
    pub fn set_option(&self, level: u64, name: u64, value: &[u8]) -> Result<bool, Errno> {
        let int = || -> Result<i32, Errno> { value.get(..4).map(|b| i32::from_le_bytes(b.try_into().expect("4"))).ok_or(EINVAL) };
        let set = |o: SocketOption| self.sock_mut().set_option(o).map(|()| true).map_err(|e| errno(&e));
        match (level, name) {
            (SOL_SOCKET, 2) => {
                let on = int()? != 0;
                self.state.lock().reuse = on;
                if self.stream { Ok(true) } else { set(SocketOption::ReuseAddress(on)) }
            }
            (SOL_SOCKET, 6) => set(SocketOption::Broadcast(int()? != 0)),
            (SOL_SOCKET, 9) => set(SocketOption::KeepAlive(int()? != 0)),
            (SOL_SOCKET, 7) => set(SocketOption::SendBuffer(usize::try_from(int()?.max(0)).unwrap_or(0))),
            (SOL_SOCKET, 8) => set(SocketOption::ReceiveBuffer(usize::try_from(int()?.max(0)).unwrap_or(0))),
            (SOL_SOCKET, 20 | 21 | 66 | 67) => {
                // struct timeval { tv_sec i64, tv_usec i64 }; zero is "never".
                if value.len() < 16 {
                    return Err(EINVAL);
                }
                let sec = i64::from_le_bytes(value[0..8].try_into().expect("8"));
                let usec = i64::from_le_bytes(value[8..16].try_into().expect("8"));
                if !(0..1_000_000).contains(&usec) {
                    return Err(crate::errno::EDOM);
                }
                // A negative time is "do not wait", as Linux takes it; zero is "never time out".
                let d = if sec < 0 { Duration::from_nanos(1) } else { Duration::from_secs(sec as u64) + Duration::from_micros(usec as u64) };
                let d = (!d.is_zero()).then_some(d);
                let mut st = self.state.lock();
                if matches!(name, 20 | 66) { st.rcvtimeo = d } else { st.sndtimeo = d }
                Ok(true)
            }
            (SOL_SOCKET, 13) => {
                if value.len() < 8 {
                    return Err(EINVAL);
                }
                let on = int()? != 0;
                let secs = i32::from_le_bytes(value[4..8].try_into().expect("4")).max(0);
                let linger = on.then(|| Duration::from_secs(secs as u64));
                // A lingering close with a time is refused by the host layer (a close cannot wait
                // here); the option is then the guest's alone, as on a datagram socket.
                match self.sock_mut().set_option(SocketOption::Linger(linger)) {
                    Err(NetError::Refused { .. }) => Ok(true),
                    r => r.map(|()| true).map_err(|e| errno(&e)),
                }
            }
            (IPPROTO_TCP, _) if !self.stream => Err(ENOPROTOOPT),
            (IPPROTO_TCP, 1) => set(SocketOption::NoDelay(int()? != 0)),
            (IPPROTO_TCP, 4) => set(SocketOption::KeepAliveIdle(Duration::from_secs(u64::try_from(int()?).map_err(|_| EINVAL)?))),
            (IPPROTO_TCP, 5) => set(SocketOption::KeepAliveInterval(Duration::from_secs(u64::try_from(int()?).map_err(|_| EINVAL)?))),
            (IPPROTO_TCP, 6) => set(SocketOption::KeepAliveCount(u32::try_from(int()?).map_err(|_| EINVAL)?)),
            (IPPROTO_IPV6, _) if self.domain != AF_INET6 => Err(ENOPROTOOPT),
            (IPPROTO_IPV6, 26) => set(SocketOption::V6Only(int()? != 0)),
            _ => Ok(false),
        }
    }

    /// What the socket is ready for, as `POLL*` bits. A stream socket that is neither connected,
    /// connecting nor listening is `POLLOUT | POLLHUP`, as Linux answers for it (Winsock's
    /// readiness call says nothing about such a socket).
    #[must_use]
    pub fn readiness(&self) -> u32 {
        let bits = self.bits(|| {
            let s = self.sock.read();
            let mut e = [PollEntry::new(&s, Interest::BOTH)];
            platnet::poll(&mut e, Duration::ZERO).ok().map(|_| e[0].readiness())
        });
        // What anyone last saw, the watcher or a guest: a guest that sees a change records it, so
        // the watcher compares its next look with this one (a socket drained and refilled between
        // two of the watcher's looks is still a change to it), wakes the watcher to watch for
        // what the socket now lacks, and wakes other waiters on this socket.
        if self.seen.swap(bits, Ordering::SeqCst) != bits {
            if crate::poll::KEYED.load(Ordering::Relaxed) {
                crate::poll::notify_key(self.key());
            } else {
                crate::poll::notify();
            }
            wake();
        }
        bits
    }

    fn bits(&self, host: impl FnOnce() -> Option<platnet::Readiness>) -> u32 {
        if self.state.lock().refused {
            return IN | OUT | ERR | HUP;
        }
        let (idle, shut_rd) = {
            let st = self.state.lock();
            (self.stream && !st.connected && !st.connecting && !st.listening, st.shut_rd)
        };
        let rd = if shut_rd { IN } else { 0 };
        if idle {
            return OUT | HUP | rd;
        }
        let Some(r) = host() else { return IN | OUT | ERR };
        let mut bits = rd;
        if r.readable {
            bits |= IN;
        }
        if r.writable {
            bits |= OUT;
        }
        if r.hangup {
            bits |= HUP;
        }
        if r.error {
            // A failed connect: Linux answers it readable, writable, in error and hung up.
            bits |= IN | OUT | ERR | HUP;
        }
        bits
    }

    /// What the watcher waits on for this socket: what it is not yet ready for. Nothing when it
    /// cannot change without a guest call (idle, hung up, in error).
    fn interest(&self, bits: u32) -> Option<Interest> {
        if bits & (ERR | HUP) != 0 {
            return None;
        }
        let (listening, active) = {
            let st = self.state.lock();
            (st.listening, st.connected || st.connecting || !self.stream)
        };
        let readable = bits & IN == 0;
        let writable = bits & OUT == 0 && active && !listening;
        (readable || writable).then_some(Interest { readable, writable })
    }
}

// ------------------------------------------------------------------------------------ watcher

static HOSTS: Mutex<Vec<Weak<Host>>> = Mutex::new(Vec::new());
static WAKE_PENDING: AtomicBool = AtomicBool::new(false);
static WAKER: std::sync::OnceLock<Option<platnet::Socket>> = std::sync::OnceLock::new();

fn watch(host: &Arc<Host>) {
    HOSTS.lock().push(Arc::downgrade(host));
    wake();
}

/// Ask the watcher to look again (a socket was made, closed, drained, or started a connect).
pub fn wake() {
    let Some(waker) = WAKER.get_or_init(start_watcher) else { return };
    if !WAKE_PENDING.swap(true, Ordering::SeqCst) && waker.send(&[1]).is_err() {
        WAKE_PENDING.store(false, Ordering::SeqCst);
    }
}

/// The watcher's loopback socket and the thread that waits on it; `None` when the host will not
/// make them (waiters then see changes at `crate::poll`'s slice).
fn start_watcher() -> Option<platnet::Socket> {
    let loopback = Arc::new(NetPolicy::loopback_only());
    let mut inbox = platnet::Socket::new(SocketKind::Datagram, IpFamily::V4, Arc::clone(&loopback)).ok()?;
    inbox.bind(&SocketAddress::loopback(IpFamily::V4, 0)).ok()?;
    inbox.set_nonblocking(true).ok()?;
    let at = inbox.local_address().ok()?;
    let mut waker = platnet::Socket::new(SocketKind::Datagram, IpFamily::V4, loopback).ok()?;
    waker.connect(&at).ok()?;
    waker.set_nonblocking(true).ok()?;
    std::thread::Builder::new().name("omni-net-watch".into()).spawn(move || run_watcher(&inbox)).ok()?;
    Some(waker)
}

fn run_watcher(inbox: &platnet::Socket) {
    let mut drain = [0u8; 64];
    loop {
        while inbox.recv(&mut drain).is_ok() {}
        WAKE_PENDING.store(false, Ordering::SeqCst);
        let live: Vec<Arc<Host>> = {
            let mut hosts = HOSTS.lock();
            hosts.retain(|h| h.strong_count() > 0);
            hosts.iter().filter_map(Weak::upgrade).collect()
        };
        let guards: Vec<_> = live.iter().map(|h| h.sock.read()).collect();
        let mut now: Vec<PollEntry<'_>> = guards.iter().map(|g| PollEntry::new(g, Interest::BOTH)).collect();
        let polled = platnet::poll(&mut now, Duration::ZERO).is_ok();
        let mut changed = Vec::new();
        let mut watched = Vec::new();
        for (i, host) in live.iter().enumerate() {
            let bits = host.bits(|| polled.then(|| now[i].readiness()));
            if host.seen.swap(bits, Ordering::SeqCst) != bits {
                changed.push(host.key());
            }
            if let Some(interest) = host.interest(bits) {
                watched.push(PollEntry::new(&guards[i], interest));
            }
        }
        drop(now);
        // `poll_keyed`: each changed socket's waiters; else every waiter in the process.
        if crate::poll::KEYED.load(Ordering::Relaxed) {
            if !changed.is_empty() {
                crate::poll::notify_keys(&changed);
            }
        } else if !changed.is_empty() {
            crate::poll::notify();
        }
        watched.push(PollEntry::new(inbox, Interest::READABLE));
        if platnet::poll(&mut watched, Duration::from_secs(1)).is_err() {
            // The host's readiness call failed: look again after a moment rather than spin.
            drop(watched);
            drop(guards);
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
