//! macOS backend for the network seam: the calls `std::net` has no spelling for.
//!
//! Implemented and run on macOS 26.5. What differs from Linux, and is handled here rather than
//! assumed:
//!
//! * **`socket(2)` has no `SOCK_NONBLOCK` and no `SOCK_CLOEXEC`.** `FD_CLOEXEC` is an `fcntl` after
//!   the fact, which leaves a window in which a `fork` would inherit the descriptor. Nothing in this
//!   runtime forks.
//! * **`SIGPIPE` is the trap on this target.** Writing to a socket whose peer has gone raises
//!   `SIGPIPE`, whose default disposition kills the process with no error, no log line and no
//!   unwind. Linux offers `MSG_NOSIGNAL` per call; macOS has only `SO_NOSIGPIPE`, so every socket
//!   this file creates or accepts gets it before it is handed out (`std`'s own sockets do the same).
//! * **The option numbers are this host's**, from `libc`'s Darwin table, never the guest's: the
//!   keep-alive idle time is `TCP_KEEPALIVE` (0x10) here, `TCP_KEEPIDLE` (4) on Linux.
//! * **Path-MTU discovery has two states here, not four.** macOS has `IP_DONTFRAG`/`IPV6_DONTFRAG`
//!   (don't-fragment on or off) and no `IP_MTU_DISCOVER`. `Dont` is don't-fragment off; `Do` and
//!   `Probe` both set it, and the host then refuses a datagram larger than the route's MTU with
//!   `EMSGSIZE` -- which is `Do`'s behaviour. `Probe`'s "ignore the discovered path MTU" has no
//!   spelling on this host, and this difference is **named, not hidden**: the mode a caller set is
//!   kept beside the host's bit (see [`PathMtuRecord`]) so that reading it back answers what was
//!   set only while the host's bit still agrees with it.
//! * **`IPV6_V6ONLY` defaults to 1**, as on Windows, where Linux takes it from a sysctl that is
//!   almost always 0. A v6 socket here does not carry v4 traffic unless the option is cleared.
//! * **`poll(2)` is the readiness call**, and it reports what `select` on Windows could not: a
//!   refused connect is `POLLOUT | POLLERR | POLLHUP`, and a peer that closed is `POLLHUP`, so
//!   [`Readiness::hangup`] is the host's answer here rather than always false.

use std::collections::HashMap;
use std::net::{TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::Mutex;
use std::time::Duration;

use super::{
    Buffer, ConnectProgress, Inner, IpFamily, NetError, NetErrorKind, NetResult, PathMtu,
    PollEntry, Readiness, SocketAddress,
};

/// The host descriptor under a socket.
fn raw(inner: &Inner) -> RawFd {
    match inner {
        Inner::Tcp(stream) => stream.as_raw_fd(),
        Inner::Udp(socket) => socket.as_raw_fd(),
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Classify a Darwin `errno` into the kind an errno is derived from, for the calls `std` does not
/// classify. The table is this host's numbers, by name from `libc`.
pub(super) fn kind_from_raw(code: i32) -> NetErrorKind {
    match code {
        libc::EWOULDBLOCK => NetErrorKind::WouldBlock,
        libc::EINPROGRESS | libc::EALREADY => NetErrorKind::InProgress,
        libc::EISCONN => NetErrorKind::AlreadyConnected,
        libc::ENOTCONN => NetErrorKind::NotConnected,
        libc::ECONNREFUSED => NetErrorKind::ConnectionRefused,
        libc::ECONNRESET => NetErrorKind::ConnectionReset,
        libc::ECONNABORTED => NetErrorKind::ConnectionAborted,
        libc::EADDRINUSE => NetErrorKind::AddressInUse,
        libc::EADDRNOTAVAIL => NetErrorKind::AddressNotAvailable,
        libc::ENETUNREACH => NetErrorKind::NetworkUnreachable,
        libc::EHOSTUNREACH => NetErrorKind::HostUnreachable,
        libc::ETIMEDOUT => NetErrorKind::TimedOut,
        // A send after `shutdown(SHUT_WR)`: Darwin answers EPIPE (with SO_NOSIGPIPE, instead of
        // the signal), and ESHUTDOWN is its older spelling of the same situation.
        libc::EPIPE | libc::ESHUTDOWN => NetErrorKind::BrokenPipe,
        libc::EACCES | libc::EPERM => NetErrorKind::PermissionDenied,
        libc::EINVAL => NetErrorKind::InvalidInput,
        libc::EAFNOSUPPORT => NetErrorKind::AddressFamilyNotSupported,
        libc::EMSGSIZE => NetErrorKind::MessageSize,
        libc::EINTR => NetErrorKind::Interrupted,
        libc::ENOBUFS => NetErrorKind::NoBufferSpace,
        _ => NetErrorKind::Other,
    }
}

fn os_error(operation: &'static str, endpoint: impl Into<String>, api: &str) -> NetError {
    let code = errno();
    NetError::kinded(operation, endpoint, kind_from_raw(code), format!("{api} failed with errno {code}"))
}

const fn address_family(family: IpFamily) -> libc::c_int {
    match family {
        IpFamily::V4 => libc::AF_INET,
        IpFamily::V6 => libc::AF_INET6,
    }
}

/// A `sockaddr` of the right family and its meaningful length. A union so the `u32` fields of
/// `sockaddr_in6` are aligned.
#[repr(C)]
union SockAddr {
    v4: libc::sockaddr_in,
    v6: libc::sockaddr_in6,
}

/// Marshal an address. **The port is byte-swapped here**, as in the Windows backend. Darwin's
/// `sockaddr`s carry a length byte (`sin_len`) that Linux's do not, and it is filled.
fn sockaddr(address: &SocketAddress) -> (SockAddr, libc::socklen_t) {
    // SAFETY: plain C data with no invalid bit patterns; zeroed so padding and `sin_zero` are
    // defined.
    let mut storage: SockAddr = unsafe { core::mem::zeroed() };
    match *address {
        SocketAddress::V4 { address: octets, port } => {
            let len = core::mem::size_of::<libc::sockaddr_in>();
            storage.v4 = libc::sockaddr_in {
                sin_len: len as u8,
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: port.to_be(),
                // The bytes in written order viewed as one word: `from_ne_bytes`, not `_be_`.
                sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(octets) },
                sin_zero: [0; 8],
            };
            (storage, len as libc::socklen_t)
        }
        SocketAddress::V6 { address: octets, port, flowinfo, scope_id } => {
            let len = core::mem::size_of::<libc::sockaddr_in6>();
            storage.v6 = libc::sockaddr_in6 {
                sin6_len: len as u8,
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: port.to_be(),
                sin6_flowinfo: flowinfo,
                sin6_addr: libc::in6_addr { s6_addr: octets },
                sin6_scope_id: scope_id,
            };
            (storage, len as libc::socklen_t)
        }
    }
}

fn set_int(fd: RawFd, level: libc::c_int, name: libc::c_int, value: libc::c_int) -> Result<(), i32> {
    // SAFETY: `value` is a live int that outlives the call and the length is its size.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            core::ptr::addr_of!(value).cast(),
            core::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(errno());
    }
    Ok(())
}

fn get_int(fd: RawFd, level: libc::c_int, name: libc::c_int) -> Result<libc::c_int, i32> {
    let mut value: libc::c_int = 0;
    let mut len = core::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value` and `len` are live locals; `len` is `value`'s size.
    let rc = unsafe {
        libc::getsockopt(fd, level, name, core::ptr::addr_of_mut!(value).cast(), &mut len)
    };
    if rc != 0 {
        return Err(errno());
    }
    Ok(value)
}

/// Make a fresh descriptor safe to hand out: close-on-exec, and no `SIGPIPE` (see the header).
fn prepare(fd: RawFd, operation: &'static str, what: &str) -> NetResult<()> {
    // SAFETY: F_SETFD on a descriptor this file owns; no memory crosses.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(os_error(operation, what, "fcntl(F_SETFD, FD_CLOEXEC)"));
    }
    set_int(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 1).map_err(|code| {
        NetError::kinded(
            operation,
            what,
            kind_from_raw(code),
            format!("setsockopt(SO_NOSIGPIPE) failed with errno {code}"),
        )
    })
}

fn create(family: IpFamily, kind: libc::c_int, what: String) -> NetResult<RawFd> {
    // SAFETY: three integers, no memory. Protocol 0 is the type's default: TCP or UDP.
    let fd = unsafe { libc::socket(address_family(family), kind, 0) };
    if fd < 0 {
        return Err(os_error("socket", what, "socket"));
    }
    PathMtuRecord::born(fd);
    if let Err(error) = prepare(fd, "socket", &what) {
        // SAFETY: the descriptor was just created here and nothing else holds it.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    Ok(fd)
}

/// `socket(AF_*, SOCK_STREAM, 0)`: unconnected, adopted into a [`TcpStream`].
pub(super) fn create_stream(family: IpFamily) -> NetResult<TcpStream> {
    let fd = create(family, libc::SOCK_STREAM, format!("{family} stream socket"))?;
    // SAFETY: a live socket this function created and owns alone; the stream closes it on drop.
    Ok(unsafe { TcpStream::from_raw_fd(fd) })
}

/// `socket(AF_*, SOCK_DGRAM, 0)`: unbound, adopted into a [`UdpSocket`].
pub(super) fn create_datagram(family: IpFamily) -> NetResult<UdpSocket> {
    let fd = create(family, libc::SOCK_DGRAM, format!("{family} datagram socket"))?;
    // SAFETY: as `create_stream`.
    Ok(unsafe { UdpSocket::from_raw_fd(fd) })
}

pub(super) fn bind(inner: &Inner, address: &SocketAddress) -> NetResult<()> {
    let (storage, len) = sockaddr(address);
    // SAFETY: `storage` is a filled sockaddr of `len` bytes that outlives the call.
    let rc = unsafe { libc::bind(raw(inner), core::ptr::addr_of!(storage).cast(), len) };
    if rc != 0 {
        return Err(os_error("bind", address.to_string(), "bind"));
    }
    Ok(())
}

pub(super) fn listen(inner: &Inner, backlog: i32) -> NetResult<()> {
    // SAFETY: a live socket and an integer.
    if unsafe { libc::listen(raw(inner), backlog) } != 0 {
        return Err(os_error("listen", format!("backlog {backlog}"), "listen"));
    }
    Ok(())
}

/// `accept(2)`. The new descriptor is **not** close-on-exec, which is Linux's `accept` (the seam
/// documents it so); it does get `SO_NOSIGPIPE`, because the signal would kill this process.
pub(super) fn accept(inner: &Inner) -> NetResult<(TcpStream, SocketAddress)> {
    // SAFETY: a live socket; null address out-parameters mean "do not return the address".
    let fd = unsafe { libc::accept(raw(inner), core::ptr::null_mut(), core::ptr::null_mut()) };
    if fd < 0 {
        return Err(os_error("accept", "a listening socket", "accept"));
    }
    PathMtuRecord::born(fd);
    // SAFETY: `accept` just created `fd`; the stream owns it and closes it on every path below.
    let stream = unsafe { TcpStream::from_raw_fd(fd) };
    set_int(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 1).map_err(|code| {
        NetError::kinded(
            "accept",
            "the accepted connection",
            kind_from_raw(code),
            format!("setsockopt(SO_NOSIGPIPE) failed with errno {code}"),
        )
    })?;
    let peer = stream
        .peer_addr()
        .map_err(|error| NetError::io("accept", "the accepted connection", &error))?;
    Ok((stream, SocketAddress::from_std(peer)))
}

/// Every IPv4 and IPv6 address of every interface, in `getifaddrs(3)`'s order, **each once**.
///
/// MEASURED on this host: `getifaddrs` lists the same link-local address on two interfaces (the
/// AWDL pair, `awdl0` and `llw0`, carry one `fe80::` address between them). The two entries differ
/// only by scope, which an `IpAddr` does not carry, so as values they are one address and it is
/// listed once, at its first position.
pub(super) fn interface_addresses() -> NetResult<Vec<std::net::IpAddr>> {
    let mut list: *mut libc::ifaddrs = core::ptr::null_mut();
    // SAFETY: `list` receives a linked list that `freeifaddrs` releases below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Err(os_error("interface_addresses", "the host's interfaces", "getifaddrs"));
    }
    let mut addresses = Vec::new();
    let mut node = list;
    while !node.is_null() {
        // SAFETY: a node of the list getifaddrs returned, alive until freeifaddrs.
        let entry = unsafe { &*node };
        let address = entry.ifa_addr;
        if !address.is_null() {
            // SAFETY: a sockaddr whose family is its first field on every family; each arm reads
            // only the variant its family names.
            unsafe {
                match i32::from((*address).sa_family) {
                    libc::AF_INET => {
                        let v4 = &*address.cast::<libc::sockaddr_in>();
                        let ip = std::net::IpAddr::from(v4.sin_addr.s_addr.to_ne_bytes());
                        if !addresses.contains(&ip) {
                            addresses.push(ip);
                        }
                    }
                    libc::AF_INET6 => {
                        let v6 = &*address.cast::<libc::sockaddr_in6>();
                        let ip = std::net::IpAddr::from(v6.sin6_addr.s6_addr);
                        if !addresses.contains(&ip) {
                            addresses.push(ip);
                        }
                    }
                    _ => {}
                }
            }
        }
        node = entry.ifa_next;
    }
    // SAFETY: the list getifaddrs returned, released exactly once.
    unsafe { libc::freeifaddrs(list) };
    Ok(addresses)
}

/// `connect(2)`; on a non-blocking socket `EINPROGRESS` is the normal answer, not a failure.
pub(super) fn start_connect(inner: &Inner, address: &SocketAddress) -> NetResult<ConnectProgress> {
    let (storage, len) = sockaddr(address);
    // SAFETY: as `bind`.
    let rc = unsafe { libc::connect(raw(inner), core::ptr::addr_of!(storage).cast(), len) };
    if rc == 0 {
        return Ok(ConnectProgress::Connected);
    }
    match errno() {
        libc::EINPROGRESS | libc::EALREADY | libc::EINTR => Ok(ConnectProgress::InProgress),
        libc::EISCONN => Ok(ConnectProgress::Connected),
        _ => Err(os_error("connect", address.to_string(), "connect")),
    }
}

fn get(inner: &Inner, level: libc::c_int, name: libc::c_int, api: &str) -> NetResult<libc::c_int> {
    get_int(raw(inner), level, name).map_err(|code| {
        NetError::kinded("getsockopt", api.to_string(), kind_from_raw(code), format!("{api} failed with errno {code}"))
    })
}

fn set(inner: &Inner, level: libc::c_int, name: libc::c_int, value: libc::c_int, api: &str) -> NetResult<()> {
    set_int(raw(inner), level, name, value).map_err(|code| {
        NetError::kinded("setsockopt", api.to_string(), kind_from_raw(code), format!("{api} failed with errno {code}"))
    })
}

/// `getsockopt(SOL_SOCKET, SO_ERROR)`: the pending error, which the host clears as it is read.
pub(super) fn socket_error(inner: &Inner) -> NetResult<Option<NetErrorKind>> {
    let value = get(inner, libc::SOL_SOCKET, libc::SO_ERROR, "getsockopt(SO_ERROR)")?;
    Ok((value != 0).then(|| kind_from_raw(value)))
}

pub(super) fn keep_alive(inner: &Inner) -> NetResult<bool> {
    Ok(get(inner, libc::SOL_SOCKET, libc::SO_KEEPALIVE, "getsockopt(SO_KEEPALIVE)")? != 0)
}

pub(super) fn set_keep_alive(inner: &Inner, on: bool) -> NetResult<()> {
    set(inner, libc::SOL_SOCKET, libc::SO_KEEPALIVE, i32::from(on), "setsockopt(SO_KEEPALIVE)")
}

fn option_i32(value: u32, option: &'static str) -> NetResult<i32> {
    i32::try_from(value).map_err(|_| {
        NetError::kinded(
            "setsockopt",
            option,
            NetErrorKind::InvalidInput,
            format!("{value} does not fit the `int` {option} is passed in; refused rather than wrapped"),
        )
    })
}

fn non_negative(value: i32, option: &'static str) -> NetResult<u32> {
    u32::try_from(value).map_err(|_| {
        NetError::kinded(
            "getsockopt",
            option,
            NetErrorKind::Other,
            format!("the host reported {value} for {option}, which is not a count of anything"),
        )
    })
}

/// `setsockopt(IPPROTO_TCP, TCP_KEEPALIVE)`: on Darwin **this** is the idle time before the first
/// probe (Linux's `TCP_KEEPIDLE`), in seconds.
pub(super) fn set_keep_alive_idle(inner: &Inner, seconds: u32) -> NetResult<()> {
    let value = option_i32(seconds, "TCP_KEEPALIVE (the keep-alive idle time)")?;
    set(inner, libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, value, "setsockopt(IPPROTO_TCP, TCP_KEEPALIVE)")
}

pub(super) fn keep_alive_idle(inner: &Inner) -> NetResult<u32> {
    let value = get(inner, libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, "getsockopt(IPPROTO_TCP, TCP_KEEPALIVE)")?;
    non_negative(value, "TCP_KEEPALIVE (the keep-alive idle time)")
}

pub(super) fn set_keep_alive_interval(inner: &Inner, seconds: u32) -> NetResult<()> {
    let value = option_i32(seconds, "TCP_KEEPINTVL (the keep-alive probe interval)")?;
    set(inner, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, value, "setsockopt(IPPROTO_TCP, TCP_KEEPINTVL)")
}

pub(super) fn keep_alive_interval(inner: &Inner) -> NetResult<u32> {
    let value = get(inner, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, "getsockopt(IPPROTO_TCP, TCP_KEEPINTVL)")?;
    non_negative(value, "TCP_KEEPINTVL (the keep-alive probe interval)")
}

pub(super) fn set_keep_alive_count(inner: &Inner, count: u32) -> NetResult<()> {
    let value = option_i32(count, "TCP_KEEPCNT (the keep-alive probe count)")?;
    set(inner, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, value, "setsockopt(IPPROTO_TCP, TCP_KEEPCNT)")
}

pub(super) fn keep_alive_count(inner: &Inner) -> NetResult<u32> {
    let value = get(inner, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, "getsockopt(IPPROTO_TCP, TCP_KEEPCNT)")?;
    non_negative(value, "TCP_KEEPCNT (the keep-alive probe count)")
}

pub(super) fn reuse_address(inner: &Inner) -> NetResult<bool> {
    Ok(get(inner, libc::SOL_SOCKET, libc::SO_REUSEADDR, "getsockopt(SO_REUSEADDR)")? != 0)
}

pub(super) fn set_reuse_address(inner: &Inner, on: bool) -> NetResult<()> {
    set(inner, libc::SOL_SOCKET, libc::SO_REUSEADDR, i32::from(on), "setsockopt(SO_REUSEADDR)")
}

fn buffer_option(which: Buffer) -> (libc::c_int, &'static str) {
    match which {
        Buffer::Receive => (libc::SO_RCVBUF, "SO_RCVBUF"),
        Buffer::Send => (libc::SO_SNDBUF, "SO_SNDBUF"),
    }
}

/// The kernel's buffer size, not the one last set: Darwin does not double it the way Linux does.
pub(super) fn buffer_bytes(inner: &Inner, which: Buffer) -> NetResult<usize> {
    let (name, label) = buffer_option(which);
    let value = get(inner, libc::SOL_SOCKET, name, label)?;
    usize::try_from(value).map_err(|_| {
        NetError::kinded(
            "getsockopt",
            label,
            NetErrorKind::Other,
            format!("the host reported a negative buffer size of {value} for {label}"),
        )
    })
}

pub(super) fn set_buffer_bytes(inner: &Inner, which: Buffer, bytes: usize) -> NetResult<()> {
    let (name, label) = buffer_option(which);
    let Ok(value) = i32::try_from(bytes) else {
        return Err(NetError::kinded(
            "setsockopt",
            label,
            NetErrorKind::InvalidInput,
            format!("{bytes} does not fit the `int` that {label} takes; refused rather than clamped"),
        ));
    };
    set(inner, libc::SOL_SOCKET, name, value, label)
}

pub(super) fn v6only(inner: &Inner) -> NetResult<bool> {
    Ok(get(inner, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, "getsockopt(IPV6_V6ONLY)")? != 0)
}

pub(super) fn set_v6only(inner: &Inner, on: bool) -> NetResult<()> {
    set(inner, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, i32::from(on), "setsockopt(IPV6_V6ONLY)")
}

/// `IP_DONTFRAG` (`<netinet/in.h>`, 28) and `IPV6_DONTFRAG` (`<netinet6/in6.h>`, 62): Darwin's
/// don't-fragment switches.
const IP_DONTFRAG: libc::c_int = 28;
const IPV6_DONTFRAG: libc::c_int = 62;

fn dont_fragment_option(family: IpFamily) -> (libc::c_int, libc::c_int, &'static str) {
    match family {
        IpFamily::V4 => (libc::IPPROTO_IP, IP_DONTFRAG, "IP_DONTFRAG"),
        IpFamily::V6 => (libc::IPPROTO_IPV6, IPV6_DONTFRAG, "IPV6_DONTFRAG"),
    }
}

/// The mode a caller set on a socket, beside the host's don't-fragment bit, because `Do` and
/// `Probe` are one bit on this host and the bit alone cannot say which was asked.
///
/// Keyed by descriptor, and **cleared when a socket is born** ([`create`] and [`accept`] are the
/// only places a socket of this seam gets its descriptor), so a descriptor number reused by a new
/// socket never inherits the old one's mode. MEASURED why the key cannot be the socket's identity:
/// a socket freed and another created at once came back with the same descriptor *and* the same
/// `st_ino`, and the new socket read back the old one's mode.
struct PathMtuRecord;

static PATH_MTU_MODES: Mutex<Option<HashMap<RawFd, PathMtu>>> = Mutex::new(None);

impl PathMtuRecord {
    fn modes() -> std::sync::MutexGuard<'static, Option<HashMap<RawFd, PathMtu>>> {
        PATH_MTU_MODES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn born(fd: RawFd) {
        if let Some(modes) = Self::modes().as_mut() {
            modes.remove(&fd);
        }
    }

    fn remember(fd: RawFd, mode: PathMtu) {
        Self::modes().get_or_insert_with(HashMap::new).insert(fd, mode);
    }

    fn recall(fd: RawFd) -> Option<PathMtu> {
        Self::modes().as_ref()?.get(&fd).copied()
    }
}

/// Don't-fragment by the socket's family: off for `Dont`, on for `Do` and `Probe` (see the module
/// header for why those two are one bit here).
pub(super) fn set_path_mtu(inner: &Inner, family: IpFamily, mode: PathMtu) -> NetResult<()> {
    let (level, name, api) = dont_fragment_option(family);
    let on = !matches!(mode, PathMtu::Dont);
    set(inner, level, name, i32::from(on), api)?;
    PathMtuRecord::remember(raw(inner), mode);
    Ok(())
}

/// The host's don't-fragment bit, reported as the mode that set it when the two agree; `None` --
/// the host's default, nothing set -- when nothing here set a mode.
pub(super) fn path_mtu(inner: &Inner, family: IpFamily) -> NetResult<Option<PathMtu>> {
    let (level, name, api) = dont_fragment_option(family);
    let on = get(inner, level, name, api)? != 0;
    Ok(match (PathMtuRecord::recall(raw(inner)), on) {
        (Some(PathMtu::Dont), false) => Some(PathMtu::Dont),
        (Some(mode @ (PathMtu::Do | PathMtu::Probe)), true) => Some(mode),
        // The bit is on and nothing here set it: `Do` is what it does on this host.
        (_, true) => Some(PathMtu::Do),
        (_, false) => None,
    })
}

/// `SO_LINGER` off, or on with a zero time -- the abortive close.
pub(super) fn set_linger(inner: &Inner, on: bool) -> NetResult<()> {
    let value = libc::linger { l_onoff: i32::from(on), l_linger: 0 };
    // SAFETY: `value` is a live `struct linger` and the length is its size.
    let rc = unsafe {
        libc::setsockopt(
            raw(inner),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            core::ptr::addr_of!(value).cast(),
            core::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(os_error("setsockopt", "SO_LINGER", "setsockopt(SO_LINGER)"));
    }
    Ok(())
}

/// `SO_LINGER_SEC`: Darwin's `SO_LINGER` reads its time in clock ticks, and this is the same option
/// in seconds, which is what the seam reports.
pub(super) fn linger(inner: &Inner) -> NetResult<Option<u16>> {
    let mut value = libc::linger { l_onoff: 0, l_linger: 0 };
    let mut len = core::mem::size_of::<libc::linger>() as libc::socklen_t;
    // SAFETY: `value` and `len` are live locals; `len` is `value`'s size.
    let rc = unsafe {
        libc::getsockopt(
            raw(inner),
            libc::SOL_SOCKET,
            libc::SO_LINGER_SEC,
            core::ptr::addr_of_mut!(value).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(os_error("getsockopt", "SO_LINGER", "getsockopt(SO_LINGER_SEC)"));
    }
    if value.l_onoff == 0 {
        return Ok(None);
    }
    u16::try_from(value.l_linger).map(Some).map_err(|_| {
        NetError::kinded(
            "getsockopt",
            "SO_LINGER",
            NetErrorKind::Other,
            format!("the host reported a linger time of {} seconds", value.l_linger),
        )
    })
}

pub(super) fn broadcast(inner: &Inner) -> NetResult<bool> {
    Ok(get(inner, libc::SOL_SOCKET, libc::SO_BROADCAST, "getsockopt(SO_BROADCAST)")? != 0)
}

pub(super) fn set_broadcast(inner: &Inner, on: bool) -> NetResult<()> {
    set(inner, libc::SOL_SOCKET, libc::SO_BROADCAST, i32::from(on), "setsockopt(SO_BROADCAST)")
}

/// Readiness over a set of sockets: `poll(2)`.
///
/// `POLLERR` is reported whatever was asked, as POSIX says. `POLLNVAL` (a descriptor that is not
/// open) is an error too; a socket held by this seam is always open, so it would mean a defect here.
///
/// **A hung-up socket is readable and writable, when either was asked.** MEASURED on this host: a
/// non-blocking connect to a loopback port with nothing listening settles with `revents ==
/// POLLHUP` and nothing else -- no `POLLOUT`, no `POLLERR` -- while `SO_ERROR` holds
/// `ECONNREFUSED`. Linux reports the same socket as `POLLIN | POLLOUT | POLLERR | POLLHUP`
/// (`tcp_poll`: a socket whose sending side is shut is reported writable so the write fails
/// rather than blocks), and "wait for writable, then read `SO_ERROR`" -- the only correct
/// non-blocking connect sequence, and the one the guest was written for -- depends on it. Both
/// halves are true of the host too: a receive on a hung-up socket returns at once (end of file or
/// the error) and so does a send (the error), so neither would block. `POLLERR` is not invented:
/// Darwin's `SO_ERROR` is cleared by the read that would reveal it, so it is left to the caller's
/// `SO_ERROR` read to report, exactly once.
pub(super) fn poll(entries: &mut [PollEntry<'_>], timeout: Duration) -> NetResult<usize> {
    let mut fds: Vec<libc::pollfd> = entries
        .iter()
        .map(|entry| {
            let mut events = 0;
            if entry.interest.readable {
                events |= libc::POLLIN;
            }
            if entry.interest.writable {
                events |= libc::POLLOUT;
            }
            libc::pollfd { fd: raw(&entry.socket.inner), events, revents: 0 }
        })
        .collect();
    // Milliseconds, rounded **up**: a sub-millisecond wait rounded down to 0 would turn a short
    // timed wait into a spin.
    let millis = timeout.as_micros().div_ceil(1000);
    let millis = libc::c_int::try_from(millis).unwrap_or(libc::c_int::MAX);
    let rc = loop {
        // SAFETY: `fds` is a live array of `fds.len()` pollfds that outlives the call.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
        // An interrupted wait is reported as nothing ready: the caller's own loop re-polls with
        // the time it has left, which is what it does after a timeout too.
        if rc < 0 && errno() == libc::EINTR {
            break 0;
        }
        break rc;
    };
    if rc < 0 {
        return Err(os_error("poll", format!("{} sockets", entries.len()), "poll"));
    }
    let mut ready = 0;
    for (entry, fd) in entries.iter_mut().zip(&fds) {
        let revents = fd.revents;
        let hangup = revents & libc::POLLHUP != 0;
        let readiness = Readiness {
            readable: revents & libc::POLLIN != 0 || (hangup && entry.interest.readable),
            writable: revents & libc::POLLOUT != 0 || (hangup && entry.interest.writable),
            hangup,
            error: revents & (libc::POLLERR | libc::POLLNVAL) != 0,
        };
        if readiness.readable || readiness.writable || readiness.hangup || readiness.error {
            ready += 1;
        }
        entry.readiness = readiness;
    }
    Ok(ready)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_connect_outcomes_are_classified_apart_from_one_another() {
        assert_eq!(kind_from_raw(libc::EINPROGRESS), NetErrorKind::InProgress);
        assert_eq!(kind_from_raw(libc::EWOULDBLOCK), NetErrorKind::WouldBlock);
        assert_eq!(kind_from_raw(libc::ECONNREFUSED), NetErrorKind::ConnectionRefused);
        assert_eq!(kind_from_raw(libc::ETIMEDOUT), NetErrorKind::TimedOut);
        assert_eq!(kind_from_raw(libc::EMSGSIZE), NetErrorKind::MessageSize);
        assert_eq!(kind_from_raw(libc::EPIPE), NetErrorKind::BrokenPipe);
        assert_eq!(kind_from_raw(9_999), NetErrorKind::Other);
    }

    #[test]
    fn a_v4_address_is_marshalled_without_reversing_its_bytes() {
        let (storage, len) = sockaddr(&SocketAddress::V4 { address: [1, 2, 3, 4], port: 443 });
        assert_eq!(len, 16);
        // SAFETY: the v4 variant was filled for a V4 address.
        unsafe {
            assert_eq!(storage.v4.sin_len, 16, "Darwin's sockaddr carries its own length");
            assert_eq!(i32::from(storage.v4.sin_family), libc::AF_INET);
            assert_eq!(storage.v4.sin_port, 443u16.to_be());
            assert_eq!(storage.v4.sin_addr.s_addr.to_ne_bytes(), [1, 2, 3, 4]);
        }
    }

    #[test]
    fn a_v6_address_keeps_its_flowinfo_and_scope() {
        let octets = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0x02, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55];
        let (storage, len) = sockaddr(&SocketAddress::V6 {
            address: octets,
            port: 49_152,
            flowinfo: 0x000a_bcde,
            scope_id: 17,
        });
        assert_eq!(len, 28);
        // SAFETY: the v6 variant was filled for a V6 address.
        unsafe {
            assert_eq!(storage.v6.sin6_len, 28);
            assert_eq!(storage.v6.sin6_port, 49_152u16.to_be());
            assert_eq!(storage.v6.sin6_flowinfo, 0x000a_bcde);
            assert_eq!(storage.v6.sin6_addr.s6_addr, octets);
            assert_eq!(storage.v6.sin6_scope_id, 17);
        }
    }
}
