//! Linux backend for the network seam.
//!
//! **Implemented, and run on Linux x86-64** (Ubuntu 26.04, kernel 7.0.0, glibc 2.43), over
//! loopback in `tests/net_loopback_linux.rs`. Not run on Linux ARM64. Most of it is the shared
//! [`unix`](super::unix) body, which is POSIX; what is here is what only Linux spells this way:
//!
//! | piece | Linux | the decision in it |
//! |---|---|---|
//! | [`create_stream`], [`create_datagram`] | `socket(af, type \| SOCK_CLOEXEC, 0)` | **close-on-exec, and blocking** |
//! | [`accept`] | `accept4(fd, NULL, NULL, SOCK_CLOEXEC)` | the same, for an accepted socket |
//! | the keep-alive timing options | `TCP_KEEPIDLE` 4, `TCP_KEEPINTVL` 5, `TCP_KEEPCNT` 6 | Linux's numbers are the guest's; they still go through a named variant |
//! | path-MTU discovery | `IP_MTU_DISCOVER` 10 / `IPV6_MTU_DISCOVER` 23; `DONT` 0, `WANT` 1, `DO` 2, `PROBE` 3 | `WANT`, the kernel's default, is the seam's "not set" |
//!
//! # Blocking, and close-on-exec, and why only one of them is a flag here
//!
//! **Blocking** is the seam's contract -- a socket starts as `socket(2)` leaves it and
//! [`Socket::set_nonblocking`](super::Socket::set_nonblocking) changes it -- so `SOCK_NONBLOCK` is
//! **not** passed even though Linux could take it for free. **Close-on-exec** is not in that
//! contract and costs nothing to have, and a descriptor that leaks into a child process is a
//! socket leaking out of an instance D30 point 3 says is isolated: `std`'s own sockets are
//! created with `SOCK_CLOEXEC` for that reason (and the Windows backend records that its `socket`
//! handle is inheritable there, and why that is not yet observable). Passing it in the type
//! argument makes it atomic, where macOS needs `fcntl` afterwards and has a window between the two.
//!
//! # The guest's numbers and the host's agree here, and must not be allowed to short-circuit
//!
//! `TCP_KEEPIDLE` is 4 on this host and in the guest's `<netinet/tcp.h>`; `IP_MTU_DISCOVER` is 10
//! in both. Nothing passes a guest number through on that account: `omni-android`'s adapter turns
//! the guest's `(level, name)` into a [`SocketOption`](super::SocketOption) and this file turns the
//! variant into the host's number, as on Windows, where the two numberings do *not* agree. A
//! pass-through that happened to be right here would be the one path that is exercised least.

use std::net::{TcpStream, UdpSocket};
use std::os::fd::FromRawFd;

use super::address::{IpFamily, SocketAddress};
use super::error::{NetError, NetErrorKind, NetResult};
use super::unix::{address_family, errno_error, get_int, raw, set_int};
use super::{Inner, PathMtu};

pub(super) use super::unix::{
    bind, broadcast, buffer_bytes, interface_addresses, keep_alive, kind_from_raw, linger, listen,
    poll, reuse_address, set_broadcast, set_buffer_bytes, set_keep_alive, set_linger,
    set_reuse_address, set_v6only, socket_error, start_connect, v6only,
};

/// `socket(af, kind | SOCK_CLOEXEC, 0)`: a new, unbound, **blocking** descriptor.
fn socket(family: IpFamily, kind: libc::c_int, what: &str) -> NetResult<libc::c_int> {
    // SAFETY: three by-value integers; no memory crosses. Protocol 0 is the type's default --
    // TCP for SOCK_STREAM, UDP for SOCK_DGRAM -- as the guest's own `socket(.., 0)` asks.
    let fd = unsafe { libc::socket(address_family(family), kind | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(errno_error("socket", format!("{family} {what} socket"), "socket"));
    }
    Ok(fd)
}

/// `socket(AF_*, SOCK_STREAM | SOCK_CLOEXEC, 0)`, adopted into a [`TcpStream`]: unconnected,
/// which `std` has no other way to produce.
pub(super) fn create_stream(family: IpFamily) -> NetResult<TcpStream> {
    let fd = socket(family, libc::SOCK_STREAM, "stream")?;
    // SAFETY: `fd` is a live socket this function just created and has given to nothing else, so
    // the `TcpStream` takes sole ownership and closes it on drop.
    Ok(unsafe { TcpStream::from_raw_fd(fd) })
}

/// `socket(AF_*, SOCK_DGRAM | SOCK_CLOEXEC, 0)`, adopted into a [`UdpSocket`]: **unbound**, as
/// `socket(2)` leaves it, which `UdpSocket::bind` would not be.
pub(super) fn create_datagram(family: IpFamily) -> NetResult<UdpSocket> {
    let fd = socket(family, libc::SOCK_DGRAM, "datagram")?;
    // SAFETY: as `create_stream`.
    Ok(unsafe { UdpSocket::from_raw_fd(fd) })
}

/// `accept4(fd, NULL, NULL, SOCK_CLOEXEC)`, adopted into a [`TcpStream`], with the peer read back
/// through `std` (`getpeername`) -- one conversion from `sockaddr` in this crate, not two.
///
/// Linux's `accept` does not pass `O_NONBLOCK` on to the new socket (`accept(2)`: "file status
/// flags such as O_NONBLOCK ... are not inherited"), and `SOCK_NONBLOCK` is not asked for, so the
/// accepted socket is blocking as the seam promises -- the host making it so rather than a flip
/// afterwards. `EINVAL` (not listening) and `EAGAIN` (nothing pending, non-blocking listener) are
/// classified by the shared table as on Windows.
pub(super) fn accept(inner: &Inner) -> NetResult<(TcpStream, SocketAddress)> {
    // SAFETY: a live descriptor; the address out-parameters are null, which `accept4` documents as
    // "do not return the address", so nothing is written through them.
    let fd = unsafe {
        libc::accept4(raw(inner), core::ptr::null_mut(), core::ptr::null_mut(), libc::SOCK_CLOEXEC)
    };
    if fd < 0 {
        return Err(errno_error("accept", "a listening socket", "accept4"));
    }
    // SAFETY: `fd` is a live socket `accept4` just created and nothing else holds; the stream owns
    // it and closes it on drop, including on the error path below.
    let stream = unsafe { TcpStream::from_raw_fd(fd) };
    let peer = stream
        .peer_addr()
        .map_err(|error| NetError::io("accept", "the accepted connection", &error))?;
    Ok((stream, SocketAddress::from_std(peer)))
}

// ====================================================================== keep-alive timing

/// A count or a number of seconds as the `int` `setsockopt` takes, or a refusal rather than a
/// wrap (`value as i32` makes a `u32` past two billion negative).
fn option_int(value: u32, option: &'static str) -> NetResult<libc::c_int> {
    libc::c_int::try_from(value).map_err(|_| {
        NetError::kinded(
            "setsockopt",
            option,
            NetErrorKind::InvalidInput,
            format!("{value} does not fit the `int` {option} is passed in; refused rather than wrapped"),
        )
    })
}

/// A host-reported count or number of seconds, or a refusal rather than `-1 as u32`.
fn non_negative(value: libc::c_int, option: &'static str) -> NetResult<u32> {
    u32::try_from(value).map_err(|_| {
        NetError::kinded(
            "getsockopt",
            option,
            NetErrorKind::Other,
            format!("the host reported {value} for {option}, which is not a count of anything"),
        )
    })
}

/// `setsockopt(IPPROTO_TCP, TCP_KEEPIDLE)`: seconds of idle before the first probe. The kernel
/// accepts `1..=32767` (`MAX_TCP_KEEPIDLE`) and answers `EINVAL` outside it, which is reported.
pub(super) fn set_keep_alive_idle(inner: &Inner, seconds: u32) -> NetResult<()> {
    let value = option_int(seconds, "TCP_KEEPIDLE")?;
    set_int(inner, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, value, "setsockopt", "setsockopt(TCP_KEEPIDLE)")
}

/// `getsockopt(IPPROTO_TCP, TCP_KEEPIDLE)`.
pub(super) fn keep_alive_idle(inner: &Inner) -> NetResult<u32> {
    let value =
        get_int(inner, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, "getsockopt", "getsockopt(TCP_KEEPIDLE)")?;
    non_negative(value, "TCP_KEEPIDLE")
}

/// `setsockopt(IPPROTO_TCP, TCP_KEEPINTVL)`: seconds between probes, `1..=32767`.
pub(super) fn set_keep_alive_interval(inner: &Inner, seconds: u32) -> NetResult<()> {
    let value = option_int(seconds, "TCP_KEEPINTVL")?;
    set_int(inner, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, value, "setsockopt", "setsockopt(TCP_KEEPINTVL)")
}

/// `getsockopt(IPPROTO_TCP, TCP_KEEPINTVL)`.
pub(super) fn keep_alive_interval(inner: &Inner) -> NetResult<u32> {
    let value =
        get_int(inner, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, "getsockopt", "getsockopt(TCP_KEEPINTVL)")?;
    non_negative(value, "TCP_KEEPINTVL")
}

/// `setsockopt(IPPROTO_TCP, TCP_KEEPCNT)`: unanswered probes before the connection is dead,
/// `1..=127` (`MAX_TCP_KEEPCNT`).
pub(super) fn set_keep_alive_count(inner: &Inner, count: u32) -> NetResult<()> {
    let value = option_int(count, "TCP_KEEPCNT")?;
    set_int(inner, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, value, "setsockopt", "setsockopt(TCP_KEEPCNT)")
}

/// `getsockopt(IPPROTO_TCP, TCP_KEEPCNT)`.
pub(super) fn keep_alive_count(inner: &Inner) -> NetResult<u32> {
    let value =
        get_int(inner, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, "getsockopt", "getsockopt(TCP_KEEPCNT)")?;
    non_negative(value, "TCP_KEEPCNT")
}

// ====================================================================== path-MTU discovery

/// The level, option and name of path-MTU discovery for a family.
const fn path_mtu_option(family: IpFamily) -> (libc::c_int, libc::c_int, &'static str) {
    match family {
        IpFamily::V4 => (libc::IPPROTO_IP, libc::IP_MTU_DISCOVER, "IP_MTU_DISCOVER"),
        IpFamily::V6 => (libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER, "IPV6_MTU_DISCOVER"),
    }
}

/// A [`PathMtu`] as this host's mode number for the family. The IPv4 and IPv6 constants are
/// equal on Linux (`DONT` 0, `DO` 2, `PROBE` 3); they are spelled per family anyway, so that the
/// equality is the headers' and not an assumption here. **Not Winsock's numbers**, where `DO` is
/// 1 and `DONT` is 2 -- which is why the seam carries the mode and never a number.
const fn path_mtu_mode(family: IpFamily, mode: PathMtu) -> libc::c_int {
    match (family, mode) {
        (IpFamily::V4, PathMtu::Dont) => libc::IP_PMTUDISC_DONT,
        (IpFamily::V4, PathMtu::Do) => libc::IP_PMTUDISC_DO,
        (IpFamily::V4, PathMtu::Probe) => libc::IP_PMTUDISC_PROBE,
        (IpFamily::V6, PathMtu::Dont) => libc::IPV6_PMTUDISC_DONT,
        (IpFamily::V6, PathMtu::Do) => libc::IPV6_PMTUDISC_DO,
        (IpFamily::V6, PathMtu::Probe) => libc::IPV6_PMTUDISC_PROBE,
    }
}

/// `setsockopt(IP_MTU_DISCOVER | IPV6_MTU_DISCOVER)` by the socket's family.
pub(super) fn set_path_mtu(inner: &Inner, family: IpFamily, mode: PathMtu) -> NetResult<()> {
    let (level, name, api) = path_mtu_option(family);
    set_int(inner, level, name, path_mtu_mode(family, mode), "setsockopt", api)
}

/// `getsockopt(IP_MTU_DISCOVER | IPV6_MTU_DISCOVER)`.
///
/// **`IP_PMTUDISC_WANT` is `None`**: it is what a fresh socket reads on this host (MEASURED, both
/// families, with `net.ipv4.ip_no_pmtu_disc = 0`), it is none of the three modes the seam can set,
/// and it is the kernel's *default* -- which is exactly what [`OptionValue::PathMtu`]'s `None`
/// is documented to mean, the role Winsock's `IP_PMTUDISC_NOT_SET` plays there. A host configured
/// with `ip_no_pmtu_disc = 1` starts sockets at `DONT` instead, and this then answers
/// `Some(Dont)`, which is what that socket does. `INTERFACE` (4) and `OMIT` (5) cannot be set
/// through the seam and are reported as an unclassified value rather than folded into a mode they
/// are not.
///
/// [`OptionValue::PathMtu`]: super::OptionValue::PathMtu
pub(super) fn path_mtu(inner: &Inner, family: IpFamily) -> NetResult<Option<PathMtu>> {
    let (level, name, api) = path_mtu_option(family);
    let value = get_int(inner, level, name, "getsockopt", api)?;
    let want = match family {
        IpFamily::V4 => libc::IP_PMTUDISC_WANT,
        IpFamily::V6 => libc::IPV6_PMTUDISC_WANT,
    };
    if value == want {
        return Ok(None);
    }
    for mode in [PathMtu::Dont, PathMtu::Do, PathMtu::Probe] {
        if value == path_mtu_mode(family, mode) {
            return Ok(Some(mode));
        }
    }
    Err(NetError::kinded(
        "getsockopt",
        api,
        NetErrorKind::Other,
        format!(
            "the host reported path-MTU discovery mode {value}, which is none of WANT, DONT, DO \
             or PROBE (INTERFACE is 4, OMIT is 5, and neither can be set through this seam)"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::{NetPolicy, Socket, SocketKind};
    use super::*;

    /// `fcntl(F_GETFL)` and `fcntl(F_GETFD)` of a socket's descriptor: (non-blocking, close-on-exec).
    fn flags(inner: &Inner) -> (bool, bool) {
        // SAFETY: a live descriptor; both are queries.
        let (fl, fd) = unsafe { (libc::fcntl(raw(inner), libc::F_GETFL), libc::fcntl(raw(inner), libc::F_GETFD)) };
        assert!(fl >= 0 && fd >= 0, "fcntl");
        (fl & libc::O_NONBLOCK != 0, fd & libc::FD_CLOEXEC != 0)
    }

    /// **A fresh socket is blocking and close-on-exec, in the kernel's own flags** -- both kinds,
    /// both families -- and `set_nonblocking` reaches the kernel's flag rather than a field.
    ///
    /// Read with `fcntl` rather than through the seam, because the seam's `nonblocking()` is a
    /// field it keeps and a socket whose field said blocking while the kernel's flag said otherwise
    /// is exactly the defect this is for.
    #[test]
    fn a_fresh_socket_is_blocking_and_close_on_exec_in_the_kernels_flags() {
        for kind in [SocketKind::Stream, SocketKind::Datagram] {
            for family in [IpFamily::V4, IpFamily::V6] {
                let mut socket =
                    Socket::new(kind, family, Arc::new(NetPolicy::loopback_only())).expect("socket");
                assert_eq!(flags(&socket.inner), (false, true), "{family} {kind:?}: fresh");
                socket.set_nonblocking(true).expect("O_NONBLOCK");
                assert_eq!(flags(&socket.inner), (true, true), "{family} {kind:?}: non-blocking");
                socket.set_nonblocking(false).expect("blocking again");
                assert_eq!(flags(&socket.inner), (false, true), "{family} {kind:?}: blocking again");
            }
        }
    }

    /// **An accepted socket is blocking and close-on-exec** although its listener is
    /// non-blocking -- the host's `accept4` making it so.
    #[test]
    fn an_accepted_socket_is_blocking_and_close_on_exec() {
        let mut listener = Socket::new(
            SocketKind::Stream,
            IpFamily::V4,
            Arc::new(NetPolicy::loopback_only()),
        )
        .expect("socket");
        listener.set_nonblocking(true).expect("non-blocking listener");
        listener.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind");
        listener.listen(4).expect("listen");
        let address = listener.local_address().expect("bound");
        let _client = std::net::TcpStream::connect(address.to_std()).expect("connect");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let accepted = loop {
            match listener.accept() {
                Ok((accepted, _)) => break accepted,
                Err(error) if error.is_would_block() && std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        assert_eq!(flags(&listener.inner), (true, true), "the listener");
        assert_eq!(flags(&accepted.inner), (false, true), "the accepted socket");
    }

    /// The mode numbers are this host's and the fresh-socket default is `WANT`, read raw.
    ///
    /// The raw read is the oracle the round trip in the loopback test cannot be: a table that
    /// swapped `DO` and `DONT` for Winsock's numbers would still round-trip through itself, and
    /// only the number the kernel holds says which mode it is in.
    #[test]
    fn the_path_mtu_modes_are_linuxs_numbers_in_the_kernel() {
        for family in [IpFamily::V4, IpFamily::V6] {
            let mut socket = Socket::new(
                SocketKind::Datagram,
                family,
                Arc::new(NetPolicy::loopback_only()),
            )
            .expect("socket");
            let (level, name, _) = path_mtu_option(family);
            let read = |s: &Socket| get_int(&s.inner, level, name, "test", "raw").expect("raw read");
            assert_eq!(read(&socket), 1, "{family}: a fresh socket is IP_PMTUDISC_WANT");
            for (mode, number) in [(PathMtu::Dont, 0), (PathMtu::Do, 2), (PathMtu::Probe, 3)] {
                socket
                    .set_option(super::super::SocketOption::PathMtuDiscovery(mode))
                    .expect("set");
                assert_eq!(read(&socket), number, "{family} {mode:?}");
            }
        }
    }

    /// The three keep-alive options land on Linux's 4, 5 and 6, read back raw by number.
    #[test]
    fn the_keep_alive_timing_options_are_linuxs_numbers_in_the_kernel() {
        let mut socket =
            Socket::new(SocketKind::Stream, IpFamily::V4, Arc::new(NetPolicy::loopback_only()))
                .expect("socket");
        use super::super::SocketOption as O;
        use std::time::Duration;
        socket.set_option(O::KeepAliveIdle(Duration::from_secs(120))).expect("idle");
        socket.set_option(O::KeepAliveInterval(Duration::from_secs(31))).expect("interval");
        socket.set_option(O::KeepAliveCount(7)).expect("count");
        let raw_read =
            |name| get_int(&socket.inner, libc::IPPROTO_TCP, name, "test", "raw").expect("raw");
        assert_eq!((raw_read(4), raw_read(5), raw_read(6)), (120, 31, 7), "TCP_KEEPIDLE/INTVL/CNT");
    }

    /// **The errno table gives Linux's codes the kinds the Windows table gives Winsock's**, named
    /// row by row (VERIFICATION entry 1: membership, not a count), and the three rows that are
    /// deliberately *not* copied stay unclassified or stay themselves.
    #[test]
    fn the_errno_table_is_the_winsock_table_in_linux_numbers() {
        use NetErrorKind as K;
        for (errno, kind) in [
            (libc::EAGAIN, K::WouldBlock),
            (libc::EWOULDBLOCK, K::WouldBlock),
            (libc::EINPROGRESS, K::InProgress),
            (libc::EALREADY, K::InProgress),
            (libc::EISCONN, K::AlreadyConnected),
            (libc::ENOTCONN, K::NotConnected),
            (libc::ECONNREFUSED, K::ConnectionRefused),
            (libc::ECONNRESET, K::ConnectionReset),
            (libc::ECONNABORTED, K::ConnectionAborted),
            (libc::EADDRINUSE, K::AddressInUse),
            (libc::EADDRNOTAVAIL, K::AddressNotAvailable),
            (libc::ENETUNREACH, K::NetworkUnreachable),
            (libc::EHOSTUNREACH, K::HostUnreachable),
            (libc::ETIMEDOUT, K::TimedOut),
            (libc::EPIPE, K::BrokenPipe),
            (libc::EACCES, K::PermissionDenied),
            (libc::EINVAL, K::InvalidInput),
            (libc::EAFNOSUPPORT, K::AddressFamilyNotSupported),
            (libc::EMSGSIZE, K::MessageSize),
            (libc::EINTR, K::Interrupted),
            (libc::ENOBUFS, K::NoBufferSpace),
            // Not copied from Windows: EPERM is not EACCES, and Linux's own EPIPE case is not
            // ESHUTDOWN. Both stay unclassified, which the adapter refuses by name.
            (libc::EPERM, K::Other),
            (libc::ESHUTDOWN, K::Other),
            (libc::ENOPROTOOPT, K::Other),
        ] {
            assert_eq!(kind_from_raw(errno), kind, "errno {errno}");
        }
    }
}
