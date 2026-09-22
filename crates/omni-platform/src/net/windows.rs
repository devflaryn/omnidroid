//! Windows backend for the network seam: the calls `std::net` has no spelling for.
//!
//! Everything else in [`crate::net`] is `std::net` and is implemented once for all five targets.
//! What is here is what needs a *different* call per target, which is D22's distinction applied
//! in the direction it points — and the list turned out longer than D30 predicted, for one
//! structural reason:
//!
//! **`std` cannot make a socket that is not already connected or bound.** `TcpStream::connect`
//! blocks until the handshake finishes, `TcpStream::connect_timeout` documents a zero `Duration`
//! as an error, and there is no constructor for a bare socket at all. A non-blocking connect is
//! the normal path for this runtime — the engine will not park a thread on one — so socket
//! creation, `bind` and `connect` are here, along with the four socket options `std` does not
//! expose and the readiness call, which is not in `std` in any form.
//!
//! Once a socket exists it is held as a [`TcpStream`] or a [`UdpSocket`], so every send, receive,
//! shutdown, timeout and non-blocking flip above this file is one portable `std` call with no
//! `cfg` anywhere near it. This file reaches back through `AsRawSocket` for the handful that are
//! not.
//!
//! # Why `select` and not `WSAPoll`
//!
//! `WSAPoll` is the obvious choice: it takes an array rather than three fixed-size sets, so it has
//! no `FD_SETSIZE` limit and it is the call a POSIX `poll` translates to line for line. It is not
//! used here, and the reason is a **documented defect** rather than a preference.
//!
//! `WSAPoll` does not report a failed connection attempt. A non-blocking `connect` that is
//! refused leaves the socket neither writable nor in error as far as `WSAPoll` is concerned, so a
//! caller waiting for the connect to settle waits for ever. Microsoft acknowledged it, declined to
//! change it for compatibility, and curl carries a note about it to this day. `select` reports the
//! same failure in `exceptfds`, which is what [`crate::net::Socket::connect_result`] reads.
//!
//! **The cost of that choice is `FD_SETSIZE`, and it is paid in the open**: an `FD_SET` holds 64
//! sockets, so [`crate::net::MAX_POLL_SOCKETS`] is 64 and a larger set is refused by name rather
//! than truncated. A truncated poll answers "not ready" about sockets it never looked at, which a
//! caller cannot tell from a timeout — and a connect that never settles is exactly the failure
//! `WSAPoll` would have introduced, arriving by the other road.
//!
//! Redefining `FD_SETSIZE` is the usual workaround and is deliberately not taken: it works because
//! Winsock's `select` reads `fd_count` and ignores the array's declared length, which is true of
//! every version anybody has tested and is not something Microsoft documents. Building the limit
//! on an undocumented property would make the refusal above a lie.
//!
//! # One difference from `std`'s own sockets, stated rather than hidden
//!
//! `std` creates its sockets with `WSASocketW` and `WSA_FLAG_NO_HANDLE_INHERIT`; this file uses
//! plain `socket(2)`, whose handle **is** inheritable by a child process. Nothing in this runtime
//! spawns a child — there is no `spawn` on any seam in this crate — so the difference is not
//! observable today. It is written down because the day something does spawn one, an inherited
//! socket is a descriptor leaking out of an instance that D30 point 3 says is isolated, and the
//! fix is `WSASocketW` with that flag rather than anything further up.

use std::net::{TcpStream, UdpSocket};
use std::os::windows::io::{AsRawSocket, FromRawSocket};
use std::sync::OnceLock;
use std::time::Duration;

use windows_sys::Win32::Networking::WinSock::{
    bind as ws_bind, connect as ws_connect, getsockopt, select as ws_select, setsockopt,
    socket as ws_socket, WSAGetLastError, WSAStartup, ADDRESS_FAMILY, AF_INET, AF_INET6, FD_SET,
    IN6_ADDR, IN6_ADDR_0, INVALID_SOCKET, IN_ADDR, IN_ADDR_0, IPPROTO_IP, IPPROTO_IPV6, IPPROTO_TCP, IPV6_DONTFRAG, IPV6_V6ONLY, IP_DONTFRAGMENT, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_IN6_0, SOCKET, SOCKET_ERROR,
    SOCK_DGRAM, SOCK_STREAM, SOL_SOCKET, SO_ERROR, SO_KEEPALIVE, SO_RCVBUF, SO_REUSEADDR,
    SO_SNDBUF, TCP_KEEPALIVE, TCP_KEEPCNT, TCP_KEEPINTVL, TIMEVAL,
    WSADATA, WSAEACCES, WSAEADDRINUSE, WSAEADDRNOTAVAIL, WSAEAFNOSUPPORT, WSAEALREADY,
    WSAECONNABORTED, WSAECONNREFUSED, WSAECONNRESET, WSAEHOSTUNREACH, WSAEINPROGRESS, WSAEINTR,
    WSAEINVAL, WSAEISCONN, WSAEMSGSIZE, WSAENETUNREACH, WSAENOBUFS, WSAENOTCONN, WSAESHUTDOWN,
    WSAETIMEDOUT, WSAEWOULDBLOCK,
};

use super::{
    Buffer, ConnectProgress, Inner, Interest, IpFamily, NetError, NetErrorKind, NetResult,
    PollEntry, Readiness, SocketAddress,
};

/// How many sockets an `FD_SET` holds: `FD_SETSIZE`.
///
/// Mirrored from `windows_sys`'s `FD_SETSIZE` as a `usize` so that the array literals below and
/// [`crate::net::MAX_POLL_SOCKETS`] are visibly the same number, and asserted equal to it in this
/// file's tests — a limit that drifted from the array it describes would overflow the array.
const FD_SET_CAPACITY: usize = 64;

/// Initialise Winsock once for this process.
///
/// `std::net` does this itself before its first socket call, and this file cannot rely on that
/// having happened: a raw `socket(2)` on a process where nothing has called `WSAStartup` fails
/// with `WSANOTINITIALISED`, and [`create_stream`] may well be the first network call the runtime
/// makes. The result is cached because `WSAStartup` is reference-counted and there is no
/// `WSACleanup` anywhere here — the library is wanted for the life of the process.
fn startup() -> NetResult<()> {
    static WINSOCK: OnceLock<i32> = OnceLock::new();
    let code = *WINSOCK.get_or_init(|| {
        // SAFETY: `WSADATA` is plain data with no invalid bit patterns, and `WSAStartup` fills it
        // entirely before returning success. Zeroing it first means a failed call leaves a
        // defined value rather than an uninitialised one.
        let mut data: WSADATA = unsafe { core::mem::zeroed() };
        // SAFETY: `data` is a live, fully-initialised `WSADATA` that outlives the call, and
        // 0x0202 is Winsock 2.2, which every supported Windows provides.
        unsafe { WSAStartup(0x0202, &mut data) }
    });
    if code == 0 {
        return Ok(());
    }
    Err(NetError::kinded(
        "socket",
        "WSAStartup",
        NetErrorKind::Other,
        format!("WSAStartup(2.2) failed with {code}, so no socket call in this process can work"),
    ))
}

/// The address family number Winsock uses. **Not the guest's**: the guest's `AF_INET6` is 10 and
/// this host's is 23, which is the disagreement `omni-android`'s adapter exists to absorb.
const fn address_family(family: IpFamily) -> ADDRESS_FAMILY {
    match family {
        IpFamily::V4 => AF_INET,
        IpFamily::V6 => AF_INET6,
    }
}

/// The host handle under a socket.
fn raw(inner: &Inner) -> SOCKET {
    match inner {
        Inner::Tcp(stream) => stream.as_raw_socket() as SOCKET,
        Inner::Udp(socket) => socket.as_raw_socket() as SOCKET,
    }
}

/// The last Winsock error, which is **not** `GetLastError` and not `errno`.
fn last_error() -> i32 {
    // SAFETY: no arguments and no memory; it reads this thread's Winsock error slot.
    unsafe { WSAGetLastError() }
}

/// Classify a Winsock error number into the kind an errno is derived from.
///
/// The numbers are Winsock's (`WSAE*` = 10000 + the BSD errno), and the mapping is to this crate's
/// own [`NetErrorKind`] rather than to a guest errno — `omni-bionic::errno` owns that table and
/// this crate sits below it. A number not in this list stays [`NetErrorKind::Other`], which the
/// adapter refuses by name rather than turning into a specific errno.
fn kind_from_wsa(code: i32) -> NetErrorKind {
    match code {
        WSAEWOULDBLOCK => NetErrorKind::WouldBlock,
        WSAEINPROGRESS | WSAEALREADY => NetErrorKind::InProgress,
        WSAEISCONN => NetErrorKind::AlreadyConnected,
        WSAENOTCONN => NetErrorKind::NotConnected,
        WSAECONNREFUSED => NetErrorKind::ConnectionRefused,
        WSAECONNRESET => NetErrorKind::ConnectionReset,
        WSAECONNABORTED => NetErrorKind::ConnectionAborted,
        WSAEADDRINUSE => NetErrorKind::AddressInUse,
        WSAEADDRNOTAVAIL => NetErrorKind::AddressNotAvailable,
        WSAENETUNREACH => NetErrorKind::NetworkUnreachable,
        WSAEHOSTUNREACH => NetErrorKind::HostUnreachable,
        WSAETIMEDOUT => NetErrorKind::TimedOut,
        // `WSAESHUTDOWN` is a send on a socket whose sending side has been shut down. Linux
        // answers `EPIPE` for the same situation, and `EPIPE` is what a caller has a branch for.
        WSAESHUTDOWN => NetErrorKind::BrokenPipe,
        WSAEACCES => NetErrorKind::PermissionDenied,
        WSAEINVAL => NetErrorKind::InvalidInput,
        WSAEAFNOSUPPORT => NetErrorKind::AddressFamilyNotSupported,
        WSAEMSGSIZE => NetErrorKind::MessageSize,
        WSAEINTR => NetErrorKind::Interrupted,
        WSAENOBUFS => NetErrorKind::NoBufferSpace,
        _ => NetErrorKind::Other,
    }
}

/// Build a [`NetError::Io`] from the thread's last Winsock error.
fn wsa_error(operation: &'static str, endpoint: impl Into<String>, api: &str) -> NetError {
    let code = last_error();
    NetError::kinded(
        operation,
        endpoint,
        kind_from_wsa(code),
        format!("{api} failed with WSAGetLastError {code}"),
    )
}

/// A `sockaddr` of the right family, and how many of its bytes are meaningful.
///
/// A union rather than a byte array, because `sockaddr_in6` contains `u32` fields and a `[u8; 28]`
/// is only byte-aligned — handing a misaligned pointer to a call that reads those fields is
/// undefined behaviour even when it happens to work.
#[repr(C)]
union SockAddr {
    v4: SOCKADDR_IN,
    v6: SOCKADDR_IN6,
}

/// Marshal one of this seam's addresses into Winsock's `sockaddr`.
///
/// **The port is byte-swapped here and nowhere else.** [`SocketAddress`] carries the port as a
/// number on purpose (see its module), so this function and the `std` conversions are the only two
/// places a `u16` becomes big-endian — which is what keeps the number of places a byte-order
/// mistake can hide down to a number somebody can check.
fn sockaddr(address: &SocketAddress) -> (SockAddr, i32) {
    // SAFETY: `SockAddr`'s two variants are plain `#[repr(C)]` data with no invalid bit patterns
    // and no `Drop`. Zeroing first means the bytes past the variant we fill — `sin_zero`, and the
    // tail of the union when the address is IPv4 — are defined rather than whatever was on the
    // stack, which matters because the length we return is the only thing stopping the OS reading
    // further.
    let mut storage: SockAddr = unsafe { core::mem::zeroed() };
    match *address {
        SocketAddress::V4 { address: octets, port } => {
            storage.v4 = SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: port.to_be(),
                sin_addr: IN_ADDR {
                    // `S_addr` is the four address bytes in the order they are written, viewed as
                    // one word. `from_ne_bytes` is therefore the *correct* conversion and
                    // `from_be_bytes` would be the plausible wrong one: it would reverse an
                    // address that is already in network order.
                    S_un: IN_ADDR_0 { S_addr: u32::from_ne_bytes(octets) },
                },
                sin_zero: [0; 8],
            };
            (storage, core::mem::size_of::<SOCKADDR_IN>() as i32)
        }
        SocketAddress::V6 { address: octets, port, flowinfo, scope_id } => {
            storage.v6 = SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: port.to_be(),
                sin6_flowinfo: flowinfo,
                sin6_addr: IN6_ADDR { u: IN6_ADDR_0 { Byte: octets } },
                Anonymous: SOCKADDR_IN6_0 { sin6_scope_id: scope_id },
            };
            (storage, core::mem::size_of::<SOCKADDR_IN6>() as i32)
        }
    }
}

/// `socket(AF_*, SOCK_STREAM, 0)`, adopted into a [`TcpStream`].
///
/// The socket is **unconnected**, which `std` has no other way to produce. Everything done to it
/// afterwards — send, receive, shutdown, timeouts, non-blocking mode — is `std`'s and is written
/// once for all five targets.
pub(super) fn create_stream(family: IpFamily) -> NetResult<TcpStream> {
    startup()?;
    // SAFETY: three by-value integers and no memory. A protocol of 0 asks the stack for the
    // default protocol of the type, which is TCP for `SOCK_STREAM` — the same thing the guest's
    // own `socket(AF_INET, SOCK_STREAM, 0)` asks for.
    let handle = unsafe { ws_socket(i32::from(address_family(family)), SOCK_STREAM, 0) };
    if handle == INVALID_SOCKET {
        return Err(wsa_error("socket", format!("{family} stream socket"), "socket"));
    }
    // SAFETY: `handle` is a live socket this function just created and has not given to anything
    // else, so `TcpStream` takes sole ownership of it and will `closesocket` it on drop.
    Ok(unsafe { TcpStream::from_raw_socket(handle as std::os::windows::raw::SOCKET) })
}

/// `socket(AF_*, SOCK_DGRAM, 0)`, adopted into a [`UdpSocket`].
///
/// **Unbound**, exactly as `socket(2)` leaves it: `getsockname` reports the wildcard address with
/// port 0 until something binds it or sends from it. `UdpSocket::bind` would have bound it here
/// and that is a different socket — one whose port is already assigned — so a guest doing
/// `socket(); setsockopt(); bind()` would be observing a state a device never shows it.
pub(super) fn create_datagram(family: IpFamily) -> NetResult<UdpSocket> {
    startup()?;
    // SAFETY: as `create_stream`. Protocol 0 is UDP for `SOCK_DGRAM`.
    let handle = unsafe { ws_socket(i32::from(address_family(family)), SOCK_DGRAM, 0) };
    if handle == INVALID_SOCKET {
        return Err(wsa_error("socket", format!("{family} datagram socket"), "socket"));
    }
    // SAFETY: as `create_stream`.
    Ok(unsafe { UdpSocket::from_raw_socket(handle as std::os::windows::raw::SOCKET) })
}

/// `bind(2)`.
pub(super) fn bind(inner: &Inner, address: &SocketAddress) -> NetResult<()> {
    let (storage, len) = sockaddr(address);
    // SAFETY: `storage` is a fully-initialised `sockaddr` of the family in `address`, living until
    // this call returns, and `len` is that variant's own size — so the call reads exactly the
    // bytes this function wrote and not one further.
    let rc = unsafe { ws_bind(raw(inner), core::ptr::addr_of!(storage).cast::<SOCKADDR>(), len) };
    if rc == SOCKET_ERROR {
        return Err(wsa_error("bind", address.to_string(), "bind"));
    }
    Ok(())
}

/// `connect(2)`, with the non-blocking case as the normal one.
///
/// On a socket that is non-blocking, a connect to anything off this machine returns
/// `WSAEWOULDBLOCK` — which is Winsock's spelling of `EINPROGRESS`, and is **not** a failure. A
/// backend that reported it as one would turn every real connection into an error, so the mapping
/// here is the load-bearing line in this file.
pub(super) fn start_connect(
    inner: &Inner,
    address: &SocketAddress,
) -> NetResult<ConnectProgress> {
    let (storage, len) = sockaddr(address);
    // SAFETY: as `bind`.
    let rc =
        unsafe { ws_connect(raw(inner), core::ptr::addr_of!(storage).cast::<SOCKADDR>(), len) };
    if rc != SOCKET_ERROR {
        return Ok(ConnectProgress::Connected);
    }
    match last_error() {
        // Winsock spells `EINPROGRESS` as `WSAEWOULDBLOCK` for `connect` specifically, and
        // `WSAEALREADY` for a second call while the first is still in flight. Both mean the
        // handshake is running and the caller should wait for writability.
        WSAEWOULDBLOCK | WSAEALREADY | WSAEINPROGRESS => Ok(ConnectProgress::InProgress),
        // A second `connect` on a socket that is already connected. POSIX says `EISCONN`, and the
        // honest answer to "start connecting" for a socket that is connected is that it is.
        WSAEISCONN => Ok(ConnectProgress::Connected),
        _ => Err(wsa_error("connect", address.to_string(), "connect")),
    }
}

/// Read an `i32`-valued socket option.
fn get_i32(
    inner: &Inner,
    level: i32,
    name: i32,
    operation: &'static str,
    api: &str,
) -> NetResult<i32> {
    let mut value: i32 = 0;
    let mut len = core::mem::size_of::<i32>() as i32;
    // SAFETY: `value` and `len` are live locals that outlive the call; `len` says the buffer is
    // four bytes and `value` is four bytes. Every option read through this helper is documented
    // by Winsock as `int`-valued, which is what makes that length correct rather than assumed.
    let rc = unsafe {
        getsockopt(
            raw(inner),
            level,
            name,
            core::ptr::addr_of_mut!(value).cast::<u8>(),
            &mut len,
        )
    };
    if rc == SOCKET_ERROR {
        return Err(wsa_error(operation, format!("option {name} at level {level}"), api));
    }
    Ok(value)
}

/// Write an `i32`-valued socket option.
fn set_i32(
    inner: &Inner,
    level: i32,
    name: i32,
    value: i32,
    operation: &'static str,
    api: &str,
) -> NetResult<()> {
    // SAFETY: `value` is a live local that outlives the call and the length is its own size.
    let rc = unsafe {
        setsockopt(
            raw(inner),
            level,
            name,
            core::ptr::addr_of!(value).cast::<u8>(),
            core::mem::size_of::<i32>() as i32,
        )
    };
    if rc == SOCKET_ERROR {
        return Err(wsa_error(operation, format!("option {name} at level {level}"), api));
    }
    Ok(())
}

/// `getsockopt(SOL_SOCKET, SO_ERROR)`: the pending socket error, **consumed by this read**.
///
/// Zero means there is none. The consumption is the host's behaviour and not this seam's, and
/// [`crate::net::Socket`] is built around it rather than hiding it: the value is moved into that
/// type's pending slot so it is still readable exactly once by whoever asks next.
pub(super) fn socket_error(inner: &Inner) -> NetResult<Option<NetErrorKind>> {
    let value = get_i32(inner, SOL_SOCKET, SO_ERROR, "getsockopt", "getsockopt(SO_ERROR)")?;
    if value == 0 {
        return Ok(None);
    }
    Ok(Some(kind_from_wsa(value)))
}

/// `getsockopt(SOL_SOCKET, SO_KEEPALIVE)`.
pub(super) fn keep_alive(inner: &Inner) -> NetResult<bool> {
    let value = get_i32(inner, SOL_SOCKET, SO_KEEPALIVE, "getsockopt", "getsockopt(SO_KEEPALIVE)")?;
    Ok(value != 0)
}

/// `setsockopt(SOL_SOCKET, SO_KEEPALIVE)`.
///
/// The boolean only. Windows carries the *timing* in `SIO_KEEPALIVE_VALS`, a `WSAIoctl` with a
/// struct, where Linux carries it in three separate `TCP_*` options -- which is why `std` has no
/// portable spelling for any of it and why [`SocketOption::KeepAlive`] promises the switch and not
/// the interval.
pub(super) fn set_keep_alive(inner: &Inner, on: bool) -> NetResult<()> {
    set_i32(
        inner,
        SOL_SOCKET,
        SO_KEEPALIVE,
        i32::from(on),
        "setsockopt",
        "setsockopt(SO_KEEPALIVE)",
    )
}

/// The three keep-alive *timing* options, which Windows numbers differently from Linux.
///
/// **This is the only place in the workspace that knows the host's numbers**, and the names below
/// come from `windows_sys::Win32::Networking::WinSock` rather than from a literal, so the compiler
/// is what keeps them right. The guest's own numbering never gets here: `omni-android`'s adapter
/// turns a `(level, optname)` pair into a [`SocketOption`](super::SocketOption) variant and this
/// file turns the variant into a host number, so there is no path along which an integer travels
/// from the guest to `setsockopt`.
///
/// The measured values in this crate's `windows-sys` (0.61.2) are `TCP_KEEPALIVE = 3`,
/// `TCP_KEEPINTVL = 17`, `TCP_KEEPCNT = 16` — against Linux's 4, 5, 6 — and the overlap that
/// makes a pass-through dangerous is `TCP_MAXRT = 5`, a live option at the same level. See
/// [`SocketOption::KeepAliveIdle`](super::SocketOption::KeepAliveIdle) for the table and for what
/// would falsify it.
///
/// All three are `DWORD`s of **seconds** (a count, for `TCP_KEEPCNT`), and all three are read and
/// written through the same `int`-sized helpers as every other option here, which is correct
/// because a `DWORD` and an `int` are both four bytes and `setsockopt`'s length argument is what
/// Winsock checks. The values are small and positive, so the sign difference cannot bite: a
/// keep-alive interval large enough to look negative as an `i32` is 68 years.
///
/// `setsockopt(IPPROTO_TCP, TCP_KEEPALIVE)`: the idle time before the first probe.
pub(super) fn set_keep_alive_idle(inner: &Inner, seconds: u32) -> NetResult<()> {
    set_i32(
        inner,
        IPPROTO_TCP,
        TCP_KEEPALIVE,
        option_i32(seconds, "TCP_KEEPALIVE (the keep-alive idle time)")?,
        "setsockopt",
        "setsockopt(IPPROTO_TCP, TCP_KEEPALIVE)",
    )
}

/// `getsockopt(IPPROTO_TCP, TCP_KEEPALIVE)`. See [`set_keep_alive_idle`].
pub(super) fn keep_alive_idle(inner: &Inner) -> NetResult<u32> {
    let value =
        get_i32(inner, IPPROTO_TCP, TCP_KEEPALIVE, "getsockopt", "getsockopt(IPPROTO_TCP, TCP_KEEPALIVE)")?;
    non_negative(value, "TCP_KEEPALIVE (the keep-alive idle time)")
}

/// `setsockopt(IPPROTO_TCP, TCP_KEEPINTVL)`: the interval between probes.
pub(super) fn set_keep_alive_interval(inner: &Inner, seconds: u32) -> NetResult<()> {
    set_i32(
        inner,
        IPPROTO_TCP,
        TCP_KEEPINTVL,
        option_i32(seconds, "TCP_KEEPINTVL (the keep-alive probe interval)")?,
        "setsockopt",
        "setsockopt(IPPROTO_TCP, TCP_KEEPINTVL)",
    )
}

/// `getsockopt(IPPROTO_TCP, TCP_KEEPINTVL)`. See [`set_keep_alive_interval`].
pub(super) fn keep_alive_interval(inner: &Inner) -> NetResult<u32> {
    let value =
        get_i32(inner, IPPROTO_TCP, TCP_KEEPINTVL, "getsockopt", "getsockopt(IPPROTO_TCP, TCP_KEEPINTVL)")?;
    non_negative(value, "TCP_KEEPINTVL (the keep-alive probe interval)")
}

/// `setsockopt(IPPROTO_TCP, TCP_KEEPCNT)`: how many probes before the connection is dead.
pub(super) fn set_keep_alive_count(inner: &Inner, count: u32) -> NetResult<()> {
    set_i32(
        inner,
        IPPROTO_TCP,
        TCP_KEEPCNT,
        option_i32(count, "TCP_KEEPCNT (the keep-alive probe count)")?,
        "setsockopt",
        "setsockopt(IPPROTO_TCP, TCP_KEEPCNT)",
    )
}

/// `getsockopt(IPPROTO_TCP, TCP_KEEPCNT)`. See [`set_keep_alive_count`].
pub(super) fn keep_alive_count(inner: &Inner) -> NetResult<u32> {
    let value =
        get_i32(inner, IPPROTO_TCP, TCP_KEEPCNT, "getsockopt", "getsockopt(IPPROTO_TCP, TCP_KEEPCNT)")?;
    non_negative(value, "TCP_KEEPCNT (the keep-alive probe count)")
}

/// A count or a number of seconds as the `int` `setsockopt` is passed, or a refusal.
///
/// The same reasoning [`set_buffer_bytes`] gives, in the other direction: `value as i32` makes a
/// `u32` above two billion negative, and Winsock would either reject it with an error naming the
/// wrong problem or take it. [`crate::net::Socket`] already bounds the seconds it is given, so
/// nothing the guest can send reaches this refusal — it is here because an `as` at a seam between
/// two integer widths is a wrap waiting for a caller this crate does not have yet.
fn option_i32(value: u32, option: &'static str) -> NetResult<i32> {
    i32::try_from(value).map_err(|_| {
        NetError::kinded(
            "setsockopt",
            option,
            NetErrorKind::InvalidInput,
            format!(
                "{value} does not fit the `int` {option} is passed in. It is refused rather than \
                 wrapped, because `value as i32` would hand Winsock a negative number for a \
                 count of seconds"
            ),
        )
    })
}

/// A host-reported option value that must be a count or a number of seconds, or a refusal.
///
/// The same reasoning [`buffer_bytes`] gives, and for the same reason it is a function rather
/// than an `as`: `value as u32` turns -1 into four billion and the caller believes it. Nothing
/// has been observed reporting a negative keep-alive figure; that is precisely why an `as` here
/// would never be noticed if something started.
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

/// `getsockopt(SOL_SOCKET, SO_REUSEADDR)`.
pub(super) fn reuse_address(inner: &Inner) -> NetResult<bool> {
    let value =
        get_i32(inner, SOL_SOCKET, SO_REUSEADDR, "getsockopt", "getsockopt(SO_REUSEADDR)")?;
    Ok(value != 0)
}

/// `setsockopt(SOL_SOCKET, SO_REUSEADDR)`.
pub(super) fn set_reuse_address(inner: &Inner, on: bool) -> NetResult<()> {
    set_i32(
        inner,
        SOL_SOCKET,
        SO_REUSEADDR,
        i32::from(on),
        "setsockopt",
        "setsockopt(SO_REUSEADDR)",
    )
}

/// `getsockopt(SOL_SOCKET, SO_RCVBUF | SO_SNDBUF)`.
///
/// The number is the **kernel's**, not the one that was last set: every stack adjusts it, and a
/// caller that reads back what it wrote is reading a value this layer invented.
pub(super) fn buffer_bytes(inner: &Inner, which: Buffer) -> NetResult<usize> {
    let name = match which {
        Buffer::Receive => SO_RCVBUF,
        Buffer::Send => SO_SNDBUF,
    };
    let value = get_i32(inner, SOL_SOCKET, name, "getsockopt", which.as_str())?;
    if value < 0 {
        // A negative buffer size is not a size. It is reported rather than cast, because
        // `value as usize` would turn -1 into 18 quintillion and the caller would believe it.
        return Err(NetError::kinded(
            "getsockopt",
            which.as_str(),
            NetErrorKind::Other,
            format!("the host reported a negative buffer size of {value} for {}", which.as_str()),
        ));
    }
    Ok(value as usize)
}

/// `setsockopt(SOL_SOCKET, SO_RCVBUF | SO_SNDBUF)`.
pub(super) fn set_buffer_bytes(inner: &Inner, which: Buffer, bytes: usize) -> NetResult<()> {
    let name = match which {
        Buffer::Receive => SO_RCVBUF,
        Buffer::Send => SO_SNDBUF,
    };
    let Ok(value) = i32::try_from(bytes) else {
        // Refused rather than clamped: a clamp sets a buffer the caller did not ask for and
        // reports success, which is the shape rule 1 exists for.
        return Err(NetError::kinded(
            "setsockopt",
            which.as_str(),
            NetErrorKind::InvalidInput,
            format!(
                "{bytes} does not fit the `int` that {} takes. It is refused rather than \
                 clamped, because a clamped buffer is a value the caller never asked for \
                 reported as though it had been set",
                which.as_str()
            ),
        ));
    };
    set_i32(inner, SOL_SOCKET, name, value, "setsockopt", which.as_str())
}

/// `getsockopt(IPPROTO_IPV6, IPV6_V6ONLY)`.
pub(super) fn v6only(inner: &Inner) -> NetResult<bool> {
    let value =
        get_i32(inner, IPPROTO_IPV6, IPV6_V6ONLY, "getsockopt", "getsockopt(IPV6_V6ONLY)")?;
    Ok(value != 0)
}

/// `setsockopt(IPPROTO_IPV6, IPV6_V6ONLY)`.
pub(super) fn set_v6only(inner: &Inner, on: bool) -> NetResult<()> {
    set_i32(
        inner,
        IPPROTO_IPV6,
        IPV6_V6ONLY,
        i32::from(on),
        "setsockopt",
        "setsockopt(IPV6_V6ONLY)",
    )
}

/// `setsockopt(IPPROTO_IP, IP_DONTFRAGMENT)` or `setsockopt(IPPROTO_IPV6, IPV6_DONTFRAG)`, by the
/// socket's family -- see [`SocketOption::DontFragment`](super::SocketOption::DontFragment).
pub(super) fn set_dont_fragment(inner: &Inner, family: IpFamily, on: bool) -> NetResult<()> {
    let (level, name, api) = match family {
        IpFamily::V4 => (IPPROTO_IP, IP_DONTFRAGMENT, "setsockopt(IP_DONTFRAGMENT)"),
        IpFamily::V6 => (IPPROTO_IPV6, IPV6_DONTFRAG, "setsockopt(IPV6_DONTFRAG)"),
    };
    set_i32(inner, level, name, i32::from(on), "setsockopt", api)
}

/// Add a socket to an `FD_SET`. The caller has already bounded the count.
fn push(set: &mut FD_SET, socket: SOCKET) {
    let at = set.fd_count as usize;
    debug_assert!(at < FD_SET_CAPACITY, "the caller bounds the set at MAX_POLL_SOCKETS");
    set.fd_array[at] = socket;
    set.fd_count += 1;
}

/// Is a socket in the set `select` left behind?
///
/// `select` rewrites each set in place to hold only the sockets that are ready, so membership
/// *after* the call is the answer. Scanning is fine at this size: `FD_SETSIZE` is 64.
fn contains(set: &FD_SET, socket: SOCKET) -> bool {
    set.fd_array[..set.fd_count as usize].contains(&socket)
}

/// An empty `FD_SET`.
fn empty_set() -> FD_SET {
    FD_SET { fd_count: 0, fd_array: [0; FD_SET_CAPACITY] }
}

/// Readiness over a set of sockets, with a caller-supplied bound: `select(2)`.
///
/// Every socket goes into `exceptfds` regardless of what the caller asked about, because that is
/// where Winsock reports a **failed connect** — see this module's header for why `WSAPoll` is not
/// used. A caller that asked only about readability still needs to hear that its connection
/// attempt was refused, and it hears it as `error`.
///
/// `hangup` is never set. A TCP peer that closed makes the socket *readable*, and the zero-length
/// `recv` that follows is how end of file is reported; `select` has no separate signal for it and
/// this backend does not invent one. That is the same shape [`crate::fs::pipe`] documents for a
/// read end whose writers have gone — end of file is a read that returns immediately.
pub(super) fn poll(entries: &mut [PollEntry<'_>], timeout: Duration) -> NetResult<usize> {
    let mut read = empty_set();
    let mut write = empty_set();
    let mut except = empty_set();
    for entry in entries.iter() {
        // A field of a private type, reached from a child module: `Inner` never leaves `net`.
        let handle = raw(&entry.socket.inner);
        let Interest { readable, writable } = entry.interest;
        if readable {
            push(&mut read, handle);
        }
        if writable {
            push(&mut write, handle);
        }
        push(&mut except, handle);
    }

    // `select` on Windows ignores `nfds` entirely — it is there for source compatibility with
    // BSD, where it is the highest descriptor plus one. Zero is what every Winsock example passes.
    let seconds = i32::try_from(timeout.as_secs()).unwrap_or(i32::MAX);
    let wait = TIMEVAL { tv_sec: seconds, tv_usec: timeout.subsec_micros() as i32 };
    // SAFETY: the three sets and the timeout are live locals that outlive the call. `select`
    // reads `fd_count` from each set and rewrites it in place with the ready subset, which is why
    // they are passed by mutable pointer and read back below rather than reused.
    let rc = unsafe {
        ws_select(
            0,
            core::ptr::addr_of_mut!(read),
            core::ptr::addr_of_mut!(write),
            core::ptr::addr_of_mut!(except),
            &wait,
        )
    };
    if rc == SOCKET_ERROR {
        return Err(wsa_error("poll", format!("{} sockets", entries.len()), "select"));
    }

    let mut ready = 0;
    for entry in entries.iter_mut() {
        let handle = raw(&entry.socket.inner);
        let readiness = Readiness {
            readable: contains(&read, handle),
            writable: contains(&write, handle),
            hangup: false,
            error: contains(&except, handle),
        };
        if readiness.readable || readiness.writable || readiness.error {
            ready += 1;
        }
        entry.readiness = readiness;
    }
    Ok(ready)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The array literals in this file are exactly `FD_SETSIZE` long, and the seam's public limit
    /// is the same number.
    ///
    /// **A relation between two constants, not a literal** (VERIFICATION entry 6's remedy): if
    /// `windows-sys` ever reported a different `FD_SETSIZE`, [`push`] would write past the end of
    /// an array whose length came from here, and nothing else in this crate would notice.
    #[test]
    fn the_fd_set_capacity_is_the_hosts_and_the_public_limit_agrees_with_it() {
        assert_eq!(
            FD_SET_CAPACITY,
            windows_sys::Win32::Networking::WinSock::FD_SETSIZE as usize,
            "the array literals in this file are sized from FD_SET_CAPACITY"
        );
        assert_eq!(
            crate::net::MAX_POLL_SOCKETS,
            FD_SET_CAPACITY,
            "the refusal in net::poll must be the size of the set it protects"
        );
        assert_eq!(empty_set().fd_array.len(), FD_SET_CAPACITY);
    }

    /// The Winsock numbers this backend has to tell apart do not collapse into one kind.
    ///
    /// `WSAEWOULDBLOCK` is what a pending connect reports and `WSAECONNREFUSED` is what a failed
    /// one does; a mapping that made them the same would turn every refused connection into a
    /// caller waiting for writability that never comes.
    #[test]
    fn the_connect_outcomes_are_classified_apart_from_one_another() {
        assert_eq!(kind_from_wsa(WSAEWOULDBLOCK), NetErrorKind::WouldBlock);
        assert_eq!(kind_from_wsa(WSAEALREADY), NetErrorKind::InProgress);
        assert_eq!(kind_from_wsa(WSAECONNREFUSED), NetErrorKind::ConnectionRefused);
        assert_eq!(kind_from_wsa(WSAETIMEDOUT), NetErrorKind::TimedOut);
        assert_eq!(kind_from_wsa(WSAEHOSTUNREACH), NetErrorKind::HostUnreachable);
        assert_eq!(kind_from_wsa(WSAENETUNREACH), NetErrorKind::NetworkUnreachable);
        // A number nobody has decided about stays unclassified rather than becoming a plausible
        // errno: the adapter refuses such a call by name.
        assert_eq!(kind_from_wsa(10_999), NetErrorKind::Other);
    }

    /// The v4 address bytes reach `sin_addr` in the order they are written.
    ///
    /// `1.2.3.4` is used because its reverse, `4.3.2.1`, is a different address — a palindrome
    /// would pass against a byte-swapped implementation.
    #[test]
    fn a_v4_address_is_marshalled_without_reversing_its_bytes() {
        let (storage, len) = sockaddr(&SocketAddress::V4 { address: [1, 2, 3, 4], port: 443 });
        assert_eq!(len, 16, "sockaddr_in is 16 bytes");
        // SAFETY: `sockaddr` filled the `v4` variant for a `SocketAddress::V4`, and every field
        // read here is plain integer data.
        unsafe {
            assert_eq!(storage.v4.sin_family, AF_INET);
            assert_eq!(storage.v4.sin_port, 443_u16.to_be(), "the port goes out big-endian");
            assert_eq!(storage.v4.sin_addr.S_un.S_addr.to_ne_bytes(), [1, 2, 3, 4]);
        }
    }

    /// The v6 address carries its flowinfo and scope id into the `sockaddr`.
    ///
    /// Both are non-zero in this fixture on purpose: a marshaller that dropped either would pass a
    /// test built from zeros, which is VERIFICATION entry 1's shape.
    #[test]
    fn a_v6_address_is_marshalled_with_both_of_the_fields_that_are_easy_to_drop() {
        let octets =
            [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0x02, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55];
        let (storage, len) = sockaddr(&SocketAddress::V6 {
            address: octets,
            port: 49_152,
            flowinfo: 0x000a_bcde,
            scope_id: 17,
        });
        assert_eq!(len, 28, "sockaddr_in6 is 28 bytes on Windows");
        // SAFETY: `sockaddr` filled the `v6` variant for a `SocketAddress::V6`, and every field
        // read here is plain integer data.
        unsafe {
            assert_eq!(storage.v6.sin6_family, AF_INET6);
            assert_eq!(storage.v6.sin6_port, 49_152_u16.to_be());
            assert_eq!(storage.v6.sin6_flowinfo, 0x000a_bcde);
            assert_eq!(storage.v6.sin6_addr.u.Byte, octets);
            assert_eq!(storage.v6.Anonymous.sin6_scope_id, 17);
        }
    }

    /// The set helpers agree: what was pushed is found, and what was not is not.
    #[test]
    fn a_socket_is_found_in_a_set_it_was_pushed_into_and_not_in_one_it_was_not() {
        let mut set = empty_set();
        push(&mut set, 7);
        push(&mut set, 9);
        assert_eq!(set.fd_count, 2);
        assert!(contains(&set, 7));
        assert!(contains(&set, 9));
        assert!(!contains(&set, 8), "a socket nobody pushed must not be reported ready");
        // The tail of the array is zero and must not be scanned: socket 0 was never pushed.
        assert!(!contains(&set, 0), "the unused tail of the array is not part of the set");
    }
}
