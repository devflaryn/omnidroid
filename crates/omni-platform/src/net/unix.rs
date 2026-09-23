//! Shared unix body of the network seam, used by the [`linux`](super::linux) and
//! [`macos`](super::macos) backends.
//!
//! # Status: the POSIX calls implemented and run on Linux; four Linux-only pieces elsewhere
//!
//! **Everything in this file with a body is POSIX and has been run on Linux** (x86-64, kernel
//! 7.0, glibc 2.43): `bind`, `listen`, `connect` with its non-blocking outcome, the `SOL_SOCKET`
//! and `IPPROTO_IPV6` options both targets number through their own headers, `SO_ERROR`,
//! `poll(2)`, `getifaddrs(3)`, and the `errno` table. **None of it has been run on macOS.** It is
//! written to compile there -- every `sockaddr` is built zeroed and assigned field by field, so
//! BSD's `sin_len` is left zero (the kernel sets it from the length argument) -- and that is read,
//! not measured.
//!
//! Four pieces are **not** POSIX-common and live in [`linux`](super::linux), leaving the
//! structural refusals below for macOS's backend, which re-exports them:
//!
//! | piece | Linux | why not here |
//! |---|---|---|
//! | socket creation, `accept` | `SOCK_CLOEXEC` in `socket`'s type, `accept4` | macOS has neither and must `fcntl` afterwards, a window a `fork` could inherit through |
//! | the keep-alive timing options | `TCP_KEEPIDLE` 4, `TCP_KEEPINTVL` 5, `TCP_KEEPCNT` 6 | macOS numbers them `TCP_KEEPALIVE` 0x10, 0x101, 0x102 |
//! | path-MTU discovery | `IP_MTU_DISCOVER`/`IPV6_MTU_DISCOVER` and the `IP_PMTUDISC_*` modes | macOS has `IP_DONTFRAG`, a boolean with no `PROBE` |
//!
//! Everything else the seam offers -- `send`, `recv`, `sendto`, `recvfrom`, `shutdown`,
//! `getsockname`, `getpeername`, non-blocking mode, `TCP_NODELAY`, both timeouts and the whole of
//! [`resolve`](super::resolve) -- is `std::net` and is implemented once for all five targets.
//!
//! # Readiness, and what this target can say that Windows cannot
//!
//! `poll(2)` reports `POLLHUP` and `POLLERR` whatever was asked for, and [`poll`] passes both
//! through as [`Readiness`](super::Readiness)'s `hangup` and `error` -- where the Windows backend's
//! `select` has no hang-up signal at all and leaves `hangup` false. That is information this host
//! has, and flattening it to match Windows would discard it. Two consequences a caller meets, both
//! the kernel's (`tcp_poll`) and MEASURED in `tests/net_loopback_linux.rs`:
//!
//! * **A TCP socket nothing has connected is `POLLOUT | POLLHUP`.** It is in `TCP_CLOSE`, and Linux
//!   reports hang-up for that state -- so a fresh stream socket answers `writable` and `hangup`
//!   here, and neither on Windows.
//! * **A refused connect is `POLLERR | POLLHUP` (with `POLLOUT`)**, and `SO_ERROR` holds
//!   `ECONNREFUSED` -- which Linux **clears** on the first read, where Winsock (MEASURED on that
//!   host) does not.
//!
//! There is no `FD_SETSIZE` here: `poll` takes an array. The seam's [`MAX_POLL_SOCKETS`] is kept
//! as the one number for all targets -- it is also the descriptor ceiling, so no guest can build a
//! larger set on any host -- and so this backend never sees a set it could not serve.
//!
//! [`MAX_POLL_SOCKETS`]: super::MAX_POLL_SOCKETS

use std::net::{TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use super::address::{IpFamily, SocketAddress};
use super::error::{NetError, NetErrorKind, NetResult};
use super::{Buffer, ConnectProgress, Inner, PathMtu, PollEntry, Readiness};

// ====================================================================== the errno table

/// A host `errno` as this seam's kind: **the same kinds the Windows backend gives the same
/// failures**, number for number (`WSAE*` is 10000 plus the BSD errno, so the two tables are one
/// table in two numberings).
///
/// Every row is a `libc` constant, so the numbers are each target's own -- `ECONNREFUSED` is 111
/// on Linux and 61 on macOS -- and this function is shared for that reason. What is **not** in it,
/// deliberately:
///
/// * `EPERM`. Linux answers it where a firewall rule refuses a connect or a send; Windows has no
///   such code and its table no such row, and mapping it to [`PermissionDenied`] would hand the
///   guest `EACCES` for an `EPERM` its own kernel would have said. It stays
///   [`Other`](NetErrorKind::Other), which the adapter refuses by name.
/// * `ESHUTDOWN`. Windows maps `WSAESHUTDOWN` to [`BrokenPipe`] because Winsock answers it where
///   Linux answers `EPIPE`. Linux answers `EPIPE` itself for that case, so an `ESHUTDOWN` here is
///   some other situation nobody has classified -- [`Other`](NetErrorKind::Other).
/// * `EAGAIN` from `connect`. On Winsock `WSAEWOULDBLOCK` is how a non-blocking connect says *in
///   progress*; on Linux `EAGAIN` from `connect` means the ephemeral ports ran out. It is
///   [`WouldBlock`](NetErrorKind::WouldBlock) here, as from every other call, and
///   [`start_connect`] does **not** treat it as progress.
///
/// [`PermissionDenied`]: NetErrorKind::PermissionDenied
/// [`BrokenPipe`]: NetErrorKind::BrokenPipe
pub(super) fn kind_from_raw(code: i32) -> NetErrorKind {
    match code {
        // `EWOULDBLOCK` is `EAGAIN` on both targets; spelled once, as a guard would be unreachable.
        libc::EAGAIN => NetErrorKind::WouldBlock,
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
        libc::EPIPE => NetErrorKind::BrokenPipe,
        libc::EACCES => NetErrorKind::PermissionDenied,
        libc::EINVAL => NetErrorKind::InvalidInput,
        libc::EAFNOSUPPORT => NetErrorKind::AddressFamilyNotSupported,
        libc::EMSGSIZE => NetErrorKind::MessageSize,
        libc::EINTR => NetErrorKind::Interrupted,
        libc::ENOBUFS => NetErrorKind::NoBufferSpace,
        _ => NetErrorKind::Other,
    }
}

/// This thread's `errno`, straight after the call that set it.
pub(super) fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// A [`NetError::Io`] from the thread's `errno`, classified by [`kind_from_raw`].
pub(super) fn errno_error(operation: &'static str, endpoint: impl Into<String>, api: &str) -> NetError {
    let code = last_errno();
    NetError::kinded(
        operation,
        endpoint,
        kind_from_raw(code),
        format!("{api} failed with errno {code} ({})", std::io::Error::from_raw_os_error(code)),
    )
}

/// The host descriptor under a socket.
pub(super) fn raw(inner: &Inner) -> RawFd {
    match inner {
        Inner::Tcp(stream) => stream.as_raw_fd(),
        Inner::Udp(socket) => socket.as_raw_fd(),
    }
}

/// The address family number this host uses. **Not the guest's** on macOS (`AF_INET6` is 30
/// there, 10 on Android); on Linux they agree, which is why the adapter's conversion is the
/// identity on this target and why nothing may rely on that.
pub(super) const fn address_family(family: IpFamily) -> libc::c_int {
    match family {
        IpFamily::V4 => libc::AF_INET,
        IpFamily::V6 => libc::AF_INET6,
    }
}

// ====================================================================== sockaddr

/// A `sockaddr` of either family, aligned for its widest member.
#[repr(C)]
pub(super) union SockAddr {
    v4: libc::sockaddr_in,
    v6: libc::sockaddr_in6,
}

/// Marshal one of this seam's addresses into the host's `sockaddr`, and its length.
///
/// **Zeroed, then assigned field by field**, so that the struct compiles on both targets (BSD's
/// `sin_len`/`sin6_len` exist only there, and are left zero -- the BSD kernel sets them from the
/// length argument) and so that padding (`sin_zero`) is defined. **The port is byte-swapped here
/// and nowhere else**, as in the Windows backend; the address bytes are copied in written order,
/// which is network order already, so `s_addr` is `from_ne_bytes` -- `from_be_bytes` would be the
/// plausible wrong one and would reverse `1.2.3.4` into `4.3.2.1`.
///
/// The length is the family's own struct size: `sizeof(struct sockaddr_storage)` would pass on
/// Linux and is refused by macOS for `AF_INET`.
pub(super) fn sockaddr(address: &SocketAddress) -> (SockAddr, libc::socklen_t) {
    // SAFETY: both union members are plain C data with no invalid bit patterns.
    let mut storage: SockAddr = unsafe { core::mem::zeroed() };
    match *address {
        SocketAddress::V4 { address: octets, port } => {
            // Writing a `Copy` field of a union is safe; the v4 member is then the one read.
            storage.v4.sin_family = libc::AF_INET as libc::sa_family_t;
            storage.v4.sin_port = port.to_be();
            storage.v4.sin_addr.s_addr = u32::from_ne_bytes(octets);
            (storage, core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t)
        }
        SocketAddress::V6 { address: octets, port, flowinfo, scope_id } => {
            // As above, for the v6 member. `sin6_flowinfo` and `sin6_scope_id` are passed as the
            // numbers they are, which is what `std`'s own `SocketAddrV6` conversion and the
            // Windows backend do -- one convention for the field in every path.
            storage.v6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            storage.v6.sin6_port = port.to_be();
            storage.v6.sin6_flowinfo = flowinfo;
            storage.v6.sin6_addr.s6_addr = octets;
            storage.v6.sin6_scope_id = scope_id;
            (storage, core::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t)
        }
    }
}

// ====================================================================== bind, listen, connect

/// `bind(2)`.
pub(super) fn bind(inner: &Inner, address: &SocketAddress) -> NetResult<()> {
    let (storage, len) = sockaddr(address);
    // SAFETY: `storage` is a fully-initialised `sockaddr` of the address's family that outlives
    // the call, and `len` is that member's size, so the kernel reads only bytes written above.
    let rc = unsafe { libc::bind(raw(inner), (&raw const storage).cast::<libc::sockaddr>(), len) };
    if rc != 0 {
        return Err(errno_error("bind", address.to_string(), "bind"));
    }
    Ok(())
}

/// `listen(2)`.
pub(super) fn listen(inner: &Inner, backlog: i32) -> NetResult<()> {
    // SAFETY: a live descriptor and an integer.
    let rc = unsafe { libc::listen(raw(inner), backlog) };
    if rc != 0 {
        return Err(errno_error("listen", format!("backlog {backlog}"), "listen"));
    }
    Ok(())
}

/// `connect(2)`, with the non-blocking outcome as the normal one -- **exactly the three answers
/// the Windows backend gives**, in this host's spelling:
///
/// | host answer | Winsock's spelling | [`ConnectProgress`] |
/// |---|---|---|
/// | `0` | `0` | `Connected` |
/// | `EINPROGRESS` (non-blocking, handshake running) | `WSAEWOULDBLOCK` | `InProgress` |
/// | `EALREADY` (a second call while it runs) | `WSAEALREADY` | `InProgress` |
/// | `EISCONN` (a call once it has finished) | `WSAEISCONN` | `Connected` |
///
/// (MEASURED on Linux 7.0: the *first* call after a non-blocking connect has finished answers `0`
/// -- the kernel reports the completion once -- and only later calls answer `EISCONN`. Both are
/// `Connected`, so the difference never reaches a caller.)
/// | anything else, `EAGAIN` included | anything else | the error, classified |
///
/// **`EINPROGRESS` is success**, and the line that says so is the one that decides whether this
/// seam works at all: a connect to anything off this machine answers it, and a backend that
/// reported it as a failure would turn every real connection into a plausible error. `EAGAIN` is
/// the row that is *not* copied from Windows: see [`kind_from_raw`]. `EINTR` on a blocking
/// socket is reported as `Interrupted`: POSIX says the connection then carries on
/// asynchronously, and the caller's next `connect` answers `EALREADY`, which is `InProgress`.
pub(super) fn start_connect(inner: &Inner, address: &SocketAddress) -> NetResult<ConnectProgress> {
    let (storage, len) = sockaddr(address);
    // SAFETY: as `bind`.
    let rc =
        unsafe { libc::connect(raw(inner), (&raw const storage).cast::<libc::sockaddr>(), len) };
    if rc == 0 {
        return Ok(ConnectProgress::Connected);
    }
    match last_errno() {
        libc::EINPROGRESS | libc::EALREADY => Ok(ConnectProgress::InProgress),
        libc::EISCONN => Ok(ConnectProgress::Connected),
        _ => Err(errno_error("connect", address.to_string(), "connect")),
    }
}

// ====================================================================== options

/// Read an `int`-valued socket option.
pub(super) fn get_int(
    inner: &Inner,
    level: libc::c_int,
    name: libc::c_int,
    operation: &'static str,
    api: &str,
) -> NetResult<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = core::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value` and `len` are live locals that outlive the call, and `len` is `value`'s
    // size. Every option read through here is documented `int`-valued on both targets.
    let rc = unsafe {
        libc::getsockopt(raw(inner), level, name, (&raw mut value).cast(), &raw mut len)
    };
    if rc != 0 {
        return Err(errno_error(operation, format!("option {name} at level {level}"), api));
    }
    Ok(value)
}

/// Write an `int`-valued socket option.
pub(super) fn set_int(
    inner: &Inner,
    level: libc::c_int,
    name: libc::c_int,
    value: libc::c_int,
    operation: &'static str,
    api: &str,
) -> NetResult<()> {
    // SAFETY: `value` is a live local and the length is its own size.
    let rc = unsafe {
        libc::setsockopt(
            raw(inner),
            level,
            name,
            (&raw const value).cast(),
            core::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(errno_error(operation, format!("option {name} at level {level}"), api));
    }
    Ok(())
}

/// `getsockopt(SOL_SOCKET, SO_ERROR)`: the pending error, **cleared by this read** on Linux --
/// MEASURED in `tests/net_loopback_linux.rs`, and the opposite of what Winsock was measured doing.
/// [`Socket`](super::Socket)'s pending slot is what makes that safe: the value read here is kept
/// there, so the guest still sees it exactly once.
pub(super) fn socket_error(inner: &Inner) -> NetResult<Option<NetErrorKind>> {
    let value = get_int(inner, libc::SOL_SOCKET, libc::SO_ERROR, "getsockopt", "getsockopt(SO_ERROR)")?;
    Ok((value != 0).then(|| kind_from_raw(value)))
}

/// `getsockopt(SOL_SOCKET, SO_KEEPALIVE)`.
pub(super) fn keep_alive(inner: &Inner) -> NetResult<bool> {
    get_int(inner, libc::SOL_SOCKET, libc::SO_KEEPALIVE, "getsockopt", "getsockopt(SO_KEEPALIVE)")
        .map(|v| v != 0)
}

/// `setsockopt(SOL_SOCKET, SO_KEEPALIVE)`: the switch only; the timing is in `linux.rs`.
pub(super) fn set_keep_alive(inner: &Inner, on: bool) -> NetResult<()> {
    set_int(
        inner,
        libc::SOL_SOCKET,
        libc::SO_KEEPALIVE,
        libc::c_int::from(on),
        "setsockopt",
        "setsockopt(SO_KEEPALIVE)",
    )
}

/// `getsockopt(SOL_SOCKET, SO_REUSEADDR)`.
pub(super) fn reuse_address(inner: &Inner) -> NetResult<bool> {
    get_int(inner, libc::SOL_SOCKET, libc::SO_REUSEADDR, "getsockopt", "getsockopt(SO_REUSEADDR)")
        .map(|v| v != 0)
}

/// `setsockopt(SOL_SOCKET, SO_REUSEADDR)` -- which here lifts the `TIME_WAIT` restriction and
/// nothing more, unlike Winsock's option of the same name (see
/// [`SocketOption::ReuseAddress`](super::SocketOption::ReuseAddress)).
pub(super) fn set_reuse_address(inner: &Inner, on: bool) -> NetResult<()> {
    set_int(
        inner,
        libc::SOL_SOCKET,
        libc::SO_REUSEADDR,
        libc::c_int::from(on),
        "setsockopt",
        "setsockopt(SO_REUSEADDR)",
    )
}

/// The host option a [`Buffer`] names.
const fn buffer_option(which: Buffer) -> libc::c_int {
    match which {
        Buffer::Receive => libc::SO_RCVBUF,
        Buffer::Send => libc::SO_SNDBUF,
    }
}

/// `getsockopt(SOL_SOCKET, SO_RCVBUF | SO_SNDBUF)`: **the kernel's number, not the request.**
///
/// Linux stores **twice** what was set -- the other half is its bookkeeping overhead
/// (`socket(7)`) -- after clamping the request to `net.core.rmem_max`/`wmem_max`, and reports the
/// doubled figure. That is what is returned: halving it to "match what was asked" would report a
/// buffer the kernel does not have, and would still be wrong for a request past the clamp.
/// MEASURED in `tests/net_loopback_linux.rs`: 65,536 asked, 131,072 read back.
pub(super) fn buffer_bytes(inner: &Inner, which: Buffer) -> NetResult<usize> {
    let value = get_int(inner, libc::SOL_SOCKET, buffer_option(which), "getsockopt", which.as_str())?;
    usize::try_from(value).map_err(|_| {
        // A negative buffer size is not a size, and `as usize` would make -1 eighteen quintillion.
        NetError::kinded(
            "getsockopt",
            which.as_str(),
            NetErrorKind::Other,
            format!("the host reported a negative buffer size of {value} for {}", which.as_str()),
        )
    })
}

/// `setsockopt(SOL_SOCKET, SO_RCVBUF | SO_SNDBUF)`. A size that does not fit the `int` is refused,
/// not clamped: a clamp sets a buffer the caller did not ask for and reports success. (The
/// kernel's own clamp to `rmem_max` is the kernel's, and [`buffer_bytes`] reports its result.)
pub(super) fn set_buffer_bytes(inner: &Inner, which: Buffer, bytes: usize) -> NetResult<()> {
    let Ok(value) = libc::c_int::try_from(bytes) else {
        return Err(NetError::kinded(
            "setsockopt",
            which.as_str(),
            NetErrorKind::InvalidInput,
            format!(
                "{bytes} does not fit the `int` that {} takes. It is refused rather than clamped, \
                 because a clamped buffer is a value the caller never asked for reported as though \
                 it had been set",
                which.as_str()
            ),
        ));
    };
    set_int(inner, libc::SOL_SOCKET, buffer_option(which), value, "setsockopt", which.as_str())
}

/// `getsockopt(IPPROTO_IPV6, IPV6_V6ONLY)`. A fresh socket's value is `net.ipv6.bindv6only`
/// (0 here, MEASURED) on Linux and 1 on macOS -- a host default, which is why the option is on the
/// seam at all.
pub(super) fn v6only(inner: &Inner) -> NetResult<bool> {
    get_int(inner, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, "getsockopt", "getsockopt(IPV6_V6ONLY)")
        .map(|v| v != 0)
}

/// `setsockopt(IPPROTO_IPV6, IPV6_V6ONLY)`.
pub(super) fn set_v6only(inner: &Inner, on: bool) -> NetResult<()> {
    set_int(
        inner,
        libc::IPPROTO_IPV6,
        libc::IPV6_V6ONLY,
        libc::c_int::from(on),
        "setsockopt",
        "setsockopt(IPV6_V6ONLY)",
    )
}

/// `setsockopt(SOL_SOCKET, SO_LINGER)` with `struct linger { on, 0 }` -- off, or the abortive
/// close; the seam refuses a nonzero time above this call. Two `int`s here, where Winsock's
/// `LINGER` is two `u_short`s.
pub(super) fn set_linger(inner: &Inner, on: bool) -> NetResult<()> {
    let value = libc::linger { l_onoff: libc::c_int::from(on), l_linger: 0 };
    // SAFETY: `value` is a live local and the length is its own size, the `struct linger`
    // `socket(7)` documents for SO_LINGER.
    let rc = unsafe {
        libc::setsockopt(
            raw(inner),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&raw const value).cast(),
            core::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(errno_error("setsockopt", "SO_LINGER", "setsockopt(SO_LINGER)"));
    }
    Ok(())
}

/// `getsockopt(SOL_SOCKET, SO_LINGER)`: `None` when off, else the linger time in seconds.
pub(super) fn linger(inner: &Inner) -> NetResult<Option<u16>> {
    let mut value = libc::linger { l_onoff: 0, l_linger: 0 };
    let mut len = core::mem::size_of::<libc::linger>() as libc::socklen_t;
    // SAFETY: `value` and `len` are live locals; `len` is `value`'s size.
    let rc = unsafe {
        libc::getsockopt(
            raw(inner),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(errno_error("getsockopt", "SO_LINGER", "getsockopt(SO_LINGER)"));
    }
    if value.l_onoff == 0 {
        return Ok(None);
    }
    u16::try_from(value.l_linger).map(Some).map_err(|_| {
        NetError::kinded(
            "getsockopt",
            "SO_LINGER",
            NetErrorKind::Other,
            format!("the host reported a linger time of {} s, which the seam cannot carry", value.l_linger),
        )
    })
}

/// `getsockopt(SOL_SOCKET, SO_BROADCAST)`.
pub(super) fn broadcast(inner: &Inner) -> NetResult<bool> {
    get_int(inner, libc::SOL_SOCKET, libc::SO_BROADCAST, "getsockopt", "getsockopt(SO_BROADCAST)")
        .map(|v| v != 0)
}

/// `setsockopt(SOL_SOCKET, SO_BROADCAST)`.
pub(super) fn set_broadcast(inner: &Inner, on: bool) -> NetResult<()> {
    set_int(
        inner,
        libc::SOL_SOCKET,
        libc::SO_BROADCAST,
        libc::c_int::from(on),
        "setsockopt",
        "setsockopt(SO_BROADCAST)",
    )
}

// ====================================================================== readiness

/// A [`Duration`] as `poll`'s millisecond timeout, **rounded up**.
///
/// Up, because `poll(2)`'s contract is a *minimum* ("wait at least timeout milliseconds", POSIX),
/// and the guest's own `poll` and `select` hand this seam their timeout: a 1.5 ms `select` that
/// came back at 1 ms would be a guest-visible early timeout. The seam's promise that nothing waits
/// "longer than its caller said" is about there always being a bound, and the bound is at most
/// one millisecond past the request -- finer than the ~15.6 ms Windows' `select` rounds to. A zero
/// stays zero, which is how `Socket::readiness` asks without waiting. Past `i32::MAX` ms (24.8
/// days) is held at it: a shorter wait, never a negative one, which `poll` would read as *for
/// ever*.
fn poll_millis(timeout: Duration) -> libc::c_int {
    libc::c_int::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(libc::c_int::MAX)
}

/// Readiness over a set of sockets: `poll(2)`.
///
/// `events` is `POLLIN` and/or `POLLOUT` by the caller's interest, and the answer is read back
/// from `revents`:
///
/// | `revents` bit | [`Readiness`] | asked for? |
/// |---|---|---|
/// | `POLLIN` | `readable` | only if the caller asked |
/// | `POLLOUT` | `writable` | only if the caller asked |
/// | `POLLHUP` | `hangup` | **always** -- `poll(2)` reports it unasked |
/// | `POLLERR` | `error` | **always**, and it is how a failed connect is seen |
/// | `POLLNVAL` | -- | an error return: the descriptor is not open, which a [`Socket`](super::Socket) that owns it cannot be |
///
/// The count is the number of entries with **any** of the four set, which is `poll`'s own return
/// value. `EINTR` is retried against the caller's deadline rather than reported: the wait is this
/// seam's and not the guest's, and a signal meant for some other part of the process is not a
/// reason to cut it short.
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
    let deadline = Instant::now() + timeout;
    loop {
        let wait = poll_millis(deadline.saturating_duration_since(Instant::now()));
        // SAFETY: `fds` is a live, uniquely-borrowed array of `fds.len()` `pollfd`s; `poll` reads
        // `fd`/`events` and writes `revents` of exactly that many. The length fits `nfds_t`: the
        // seam bounds it at `MAX_POLL_SOCKETS`.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait) };
        if rc >= 0 {
            break;
        }
        if last_errno() != libc::EINTR {
            return Err(errno_error("poll", format!("{} sockets", entries.len()), "poll"));
        }
    }
    let mut ready = 0;
    for (entry, fd) in entries.iter_mut().zip(&fds) {
        if fd.revents & libc::POLLNVAL != 0 {
            return Err(NetError::kinded(
                "poll",
                format!("descriptor {}", fd.fd),
                NetErrorKind::Other,
                "poll answered POLLNVAL: the descriptor is not open, and a Socket owns its \
                 descriptor for its whole life, so something outside this seam closed it",
            ));
        }
        let readiness = Readiness {
            readable: fd.revents & libc::POLLIN != 0,
            writable: fd.revents & libc::POLLOUT != 0,
            hangup: fd.revents & libc::POLLHUP != 0,
            error: fd.revents & libc::POLLERR != 0,
        };
        if readiness.readable || readiness.writable || readiness.hangup || readiness.error {
            ready += 1;
        }
        entry.readiness = readiness;
    }
    Ok(ready)
}

// ====================================================================== interfaces

/// `getifaddrs(3)`: every `AF_INET` and `AF_INET6` address of every interface, in the order the
/// host lists them -- loopback, link-local and interfaces that are down included, as a Java
/// `NetworkInterface` walk would see them. The `AF_PACKET` (Linux) and `AF_LINK` (macOS) entries
/// that name a link rather than an address are skipped, and so is an entry with no address at all,
/// which `getifaddrs` produces for an interface that has none.
pub(super) fn interface_addresses() -> NetResult<Vec<std::net::IpAddr>> {
    let mut list: *mut libc::ifaddrs = core::ptr::null_mut();
    // SAFETY: `list` is a live local the call writes the head of an allocated list into.
    if unsafe { libc::getifaddrs(&raw mut list) } != 0 {
        return Err(errno_error("interface_addresses", "the host's interfaces", "getifaddrs"));
    }
    let mut addresses = Vec::new();
    let mut node = list;
    while !node.is_null() {
        // SAFETY: a non-null node of the list `getifaddrs` allocated, alive until `freeifaddrs`.
        let entry = unsafe { &*node };
        let address = entry.ifa_addr;
        if !address.is_null() {
            // SAFETY: `ifa_addr` points at a `sockaddr` whose family field comes first, and whose
            // full size is that family's struct -- the kernel's own record of the address.
            let family = libc::c_int::from(unsafe { (*address).sa_family });
            if family == libc::AF_INET {
                // SAFETY: an `AF_INET` `sockaddr` is a `sockaddr_in`.
                let v4 = unsafe { &*address.cast::<libc::sockaddr_in>() };
                addresses.push(std::net::IpAddr::from(v4.sin_addr.s_addr.to_ne_bytes()));
            } else if family == libc::AF_INET6 {
                // SAFETY: an `AF_INET6` `sockaddr` is a `sockaddr_in6`.
                let v6 = unsafe { &*address.cast::<libc::sockaddr_in6>() };
                addresses.push(std::net::IpAddr::from(v6.sin6_addr.s6_addr));
            }
        }
        node = entry.ifa_next;
    }
    // SAFETY: `list` is the head `getifaddrs` returned, freed once, and nothing points into it
    // any more -- every address above was copied out.
    unsafe { libc::freeifaddrs(list) };
    Ok(addresses)
}

// ====================================================================== macOS-only structural

/// The platform this backend was compiled for, for error messages.
#[cfg_attr(target_os = "linux", allow(dead_code))] // only the macOS-only refusals below use it
fn platform() -> &'static str {
    std::env::consts::OS
}

#[cfg_attr(target_os = "linux", allow(dead_code))] // only the macOS-only refusals below use it
fn unsupported<T>(operation: &'static str, intended: &'static str) -> NetResult<T> {
    Err(NetError::Unsupported { operation, intended, platform: platform() })
}

/// Structural on macOS: `socket(2)` then `fcntl(F_SETFD, FD_CLOEXEC)`, since macOS has no
/// `SOCK_CLOEXEC`. Linux's is in [`linux`](super::linux).
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn create_stream(family: IpFamily) -> NetResult<TcpStream> {
    let _ = family;
    unsupported("socket", "socket(2) with SOCK_STREAM, then fcntl(FD_CLOEXEC), on macOS")
}

/// Structural on macOS; see [`create_stream`].
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn create_datagram(family: IpFamily) -> NetResult<UdpSocket> {
    let _ = family;
    unsupported("socket", "socket(2) with SOCK_DGRAM, then fcntl(FD_CLOEXEC), on macOS")
}

/// Structural on macOS: `accept(2)` then `fcntl(F_SETFD, FD_CLOEXEC)`; Linux uses `accept4`.
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn accept(inner: &Inner) -> NetResult<(TcpStream, SocketAddress)> {
    let _ = inner;
    unsupported("accept", "accept(2), then fcntl(FD_CLOEXEC), on macOS")
}

/// Structural on macOS: `TCP_KEEPALIVE` (0x10) is the idle time there; Linux's `TCP_KEEPIDLE`
/// (4) is in [`linux`](super::linux).
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn set_keep_alive_idle(inner: &Inner, seconds: u32) -> NetResult<()> {
    let _ = (inner, seconds);
    unsupported("setsockopt", "setsockopt(2) with IPPROTO_TCP/TCP_KEEPALIVE (0x10) on macOS")
}

/// Structural on macOS; see [`set_keep_alive_idle`].
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn keep_alive_idle(inner: &Inner) -> NetResult<u32> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with IPPROTO_TCP/TCP_KEEPALIVE (0x10) on macOS")
}

/// Structural on macOS: `TCP_KEEPINTVL` is 0x101 there, 5 on Linux.
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn set_keep_alive_interval(inner: &Inner, seconds: u32) -> NetResult<()> {
    let _ = (inner, seconds);
    unsupported("setsockopt", "setsockopt(2) with IPPROTO_TCP/TCP_KEEPINTVL (0x101) on macOS")
}

/// Structural on macOS; see [`set_keep_alive_interval`].
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn keep_alive_interval(inner: &Inner) -> NetResult<u32> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with IPPROTO_TCP/TCP_KEEPINTVL (0x101) on macOS")
}

/// Structural on macOS: `TCP_KEEPCNT` is 0x102 there, 6 on Linux.
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn set_keep_alive_count(inner: &Inner, count: u32) -> NetResult<()> {
    let _ = (inner, count);
    unsupported("setsockopt", "setsockopt(2) with IPPROTO_TCP/TCP_KEEPCNT (0x102) on macOS")
}

/// Structural on macOS; see [`set_keep_alive_count`].
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn keep_alive_count(inner: &Inner) -> NetResult<u32> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with IPPROTO_TCP/TCP_KEEPCNT (0x102) on macOS")
}

/// Structural on macOS, which has `IP_DONTFRAG` (a boolean, no `PROBE`) rather than
/// `IP_MTU_DISCOVER`; Linux's is in [`linux`](super::linux).
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn set_path_mtu(inner: &Inner, family: IpFamily, mode: PathMtu) -> NetResult<()> {
    let _ = (inner, family, mode);
    unsupported("setsockopt", "IP_DONTFRAG/IPV6_DONTFRAG on macOS, which has no PROBE mode")
}

/// Structural on macOS; see [`set_path_mtu`].
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn path_mtu(inner: &Inner, family: IpFamily) -> NetResult<Option<PathMtu>> {
    let _ = (inner, family);
    unsupported("getsockopt", "IP_DONTFRAG/IPV6_DONTFRAG on macOS, which has no PROBE mode")
}
