//! Shared unix body of the network seam, used by the [`linux`](super::linux) and
//! [`macos`](super::macos) backends.
//!
//! # Status: structural, not implemented — and it is eleven calls, not the whole seam
//!
//! **Nothing in this module has ever been run.** Every function returns
//! [`NetError::Unsupported`] naming the POSIX call it intends to make, so a Linux or macOS build
//! fails at the first socket rather than appearing to work.
//!
//! What is here is exactly the set with no single portable `std` spelling. Everything else the
//! seam offers — `send`, `recv`, `sendto`, `recvfrom`, `shutdown`, `getsockname`, `getpeername`,
//! non-blocking mode, `TCP_NODELAY`, `SO_RCVTIMEO`, `SO_SNDTIMEO` and the whole of
//! [`resolve`](super::resolve) — is `std::net` and is implemented once for all five targets,
//! because D22's other half says a fabricated `Unsupported` for something `std` already does on
//! all five is a false claim in the *other* direction.
//!
//! So the honest statement about this crate's network seam on Linux and macOS is precise rather
//! than blanket: **the portable operations are written and have never been built for those
//! targets, and these eleven are not written at all and say so by name.** In practice that means
//! no socket can be created there, so the portable half is unreachable until this file exists —
//! which is worth knowing before reading the table in [`super`] as though the seam half-works.
//!
//! # Why this is free functions with fabricated bodies, and [`window::unix`] is not
//!
//! [`crate::window::unix`] gets to write `enum Window {}` — an uninhabited type whose `create`
//! refuses, so that every other operation is discharged by the compiler with `match *self {}`.
//! That is strictly better than a hand-written refusal in each one, and VERIFICATION entry 12 is
//! why: a branch no input can take is not a check.
//!
//! **It is not available here**, and the reason is structural rather than a matter of taste. This
//! backend's operations take [`Inner`](super::Inner), which holds a [`TcpStream`] or a
//! [`UdpSocket`] — types `std` provides and that exist perfectly well on unix. There is no
//! uninhabited type to make: `std::net::UdpSocket::bind` would hand one over on Linux this
//! afternoon. What is missing is not the *handle*, it is the six calls that have no `std`
//! spelling, and a free function has to have a body. So each one is written as
//! [`crate::fs::unix`]'s two are, naming what it intends to call.
//!
//! [`window::unix`]: crate::window
//!
//! # What implementing these involves
//!
//! Less than the Windows backend, and the differences between Linux and macOS are small enough to
//! name individually:
//!
//! * **`socket(2)`.** Linux has `SOCK_NONBLOCK` and `SOCK_CLOEXEC` as type flags, so a
//!   non-blocking close-on-exec socket is one call there. macOS has neither and needs
//!   `fcntl(F_SETFL, O_NONBLOCK)` and `fcntl(F_SETFD, FD_CLOEXEC)` after the fact — two extra
//!   syscalls and, more importantly, a window between them in which a `fork` would inherit the
//!   descriptor. Nothing in this runtime forks, which is what makes that window acceptable rather
//!   than a defect.
//! * **`connect(2)` on a non-blocking socket answers `EINPROGRESS`**, where Winsock answers
//!   `WSAEWOULDBLOCK`. Both mean the same thing and the mapping to
//!   [`ConnectProgress::InProgress`](super::ConnectProgress::InProgress) is the load-bearing line
//!   in either backend.
//! * **`getsockopt(SOL_SOCKET, SO_ERROR)` clears the pending error**, exactly as on Windows.
//!   [`Socket`](super::Socket) is built around that; a backend that re-read it would find zero.
//! * **`poll(2)` rather than `select(2)`, and rather than `epoll`.** `poll` is POSIX, takes an
//!   array with no `FD_SETSIZE` limit, and exists identically on both targets — so the
//!   [`MAX_POLL_SOCKETS`](super::MAX_POLL_SOCKETS) refusal the Windows backend needs would have
//!   no reason to exist here, and whoever implements this should say so rather than inheriting
//!   the limit. `epoll` is Linux-only and `kqueue` is macOS-only; using either would make these
//!   two files stop sharing a body, for a gain nothing has measured a need for.
//! * **`POLLHUP` and `POLLRDHUP` are real here.** `poll(2)` reports a peer that closed, which
//!   `select` cannot, so the `hangup` field of [`Readiness`](super::Readiness) — which the
//!   Windows backend never sets and documents why — can carry the truth on these targets. That is
//!   one of the few places a unix implementation would be *better* rather than equivalent, and it
//!   is worth not flattening to match Windows.
//! * **`SO_RCVBUF` on Linux doubles what you set**, for kernel bookkeeping, and reads back
//!   doubled. macOS does not. Neither is wrong and a test that asserted equality would fail on
//!   Linux and pass on macOS, which is why this crate's own round-trip test asserts that the
//!   kernel agreed to *something* rather than to the number it was given.

use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

use super::address::{IpFamily, SocketAddress};
use super::error::{NetError, NetErrorKind, NetResult};
use super::{Buffer, ConnectProgress, Inner, PathMtu, PollEntry};

/// The platform this backend was compiled for, for error messages.
fn platform() -> &'static str {
    std::env::consts::OS
}

/// Intended: a host `errno` as this seam's kind, for an error `std` has no `ErrorKind` for --
/// `EMSGSIZE` first, which `std` leaves uncategorised here as on Windows. Until the table is
/// written, such an error stays [`NetErrorKind::Other`], which the adapter refuses by name.
pub(super) fn kind_from_raw(_code: i32) -> NetErrorKind {
    NetErrorKind::Other
}

fn unsupported<T>(operation: &'static str, intended: &'static str) -> NetResult<T> {
    Err(NetError::Unsupported { operation, intended, platform: platform() })
}

/// Intended: `socket(AF_INET|AF_INET6, SOCK_STREAM, 0)`, adopted with `FromRawFd`.
///
/// The decision in it: **whether to ask for `SOCK_NONBLOCK` here**. Linux can; macOS cannot; and
/// the seam's contract is that a socket starts blocking, as `socket(2)` leaves it, with
/// [`Socket::set_nonblocking`](super::Socket::set_nonblocking) changing it afterwards. So the
/// answer is no — matching the contract rather than the cheapest call — and the Linux flag is
/// worth using only if `set_nonblocking` is the very next thing every caller does, which it is
/// not.
pub(super) fn create_stream(family: IpFamily) -> NetResult<TcpStream> {
    let _ = family;
    unsupported("socket", "socket(2) with SOCK_STREAM, adopted via FromRawFd")
}

/// Intended: `socket(AF_INET|AF_INET6, SOCK_DGRAM, 0)`, adopted with `FromRawFd`.
///
/// It must be **unbound**, which is why `UdpSocket::bind` is not the implementation: a bound
/// socket has a port already, and a guest doing `socket(); setsockopt(); bind()` would observe a
/// `getsockname` no device would show it.
pub(super) fn create_datagram(family: IpFamily) -> NetResult<UdpSocket> {
    let _ = family;
    unsupported("socket", "socket(2) with SOCK_DGRAM, adopted via FromRawFd")
}

/// Intended: `bind(2)`.
///
/// The decision in it: **`sockaddr_in6` is 28 bytes on both targets and the length argument is
/// not interchangeable with `sockaddr_in`'s 16.** Passing `sizeof(struct sockaddr_storage)` works
/// on Linux and is rejected by macOS for `AF_INET`, so the length has to come from the family.
pub(super) fn bind(inner: &Inner, address: &SocketAddress) -> NetResult<()> {
    let _ = (inner, address);
    unsupported("bind", "bind(2)")
}

/// Intended: `listen(2)`.
pub(super) fn listen(inner: &Inner, backlog: i32) -> NetResult<()> {
    let _ = (inner, backlog);
    unsupported("listen", "listen(2)")
}

/// Intended: `accept(2)`, adopting the new descriptor into a `TcpStream` and reading the peer
/// with `getpeername`.
pub(super) fn accept(inner: &Inner) -> NetResult<(TcpStream, SocketAddress)> {
    let _ = inner;
    unsupported("accept", "accept(2)")
}

/// Intended: `getifaddrs(3)`, every `AF_INET` and `AF_INET6` entry in its order.
pub(super) fn interface_addresses() -> NetResult<Vec<std::net::IpAddr>> {
    unsupported("interface_addresses", "getifaddrs(3)")
}

/// Intended: `connect(2)` on a non-blocking descriptor.
///
/// The decision in it, and it is the one that decides whether this seam works at all:
/// **`EINPROGRESS` is success.** A connect to anything off this machine returns `-1` with that
/// errno, the handshake carries on, and the caller waits for writability. A backend that reported
/// it as a failure would turn every real connection into an error, and the error would be
/// perfectly plausible.
pub(super) fn start_connect(
    inner: &Inner,
    address: &SocketAddress,
) -> NetResult<ConnectProgress> {
    let _ = (inner, address);
    unsupported("connect", "connect(2), mapping EINPROGRESS to ConnectProgress::InProgress")
}

/// Intended: `getsockopt(SOL_SOCKET, SO_ERROR)`.
///
/// The decision in it: **the read clears the error**, so this must be called once and its answer
/// kept. [`Socket`](super::Socket) already holds a pending slot for exactly that reason, and a
/// backend that re-read the option to double-check would find zero and report success for a
/// connection that failed.
pub(super) fn socket_error(inner: &Inner) -> NetResult<Option<NetErrorKind>> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with SOL_SOCKET/SO_ERROR")
}

/// Intended: `getsockopt(SOL_SOCKET, SO_KEEPALIVE)`.
pub(super) fn keep_alive(inner: &Inner) -> NetResult<bool> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with SOL_SOCKET/SO_KEEPALIVE")
}

/// Intended: `setsockopt(SOL_SOCKET, SO_KEEPALIVE)`.
pub(super) fn set_keep_alive(inner: &Inner, on: bool) -> NetResult<()> {
    let _ = (inner, on);
    unsupported("setsockopt", "setsockopt(2) with SOL_SOCKET/SO_KEEPALIVE")
}

/// Intended: `setsockopt(IPPROTO_TCP, TCP_KEEPIDLE)`.
///
/// **The one place where implementing this on Linux is *easier* than on Windows, and the reason
/// the seam takes a [`Duration`] anyway.** Linux numbers the three keep-alive timing options
/// `TCP_KEEPIDLE` = 4, `TCP_KEEPINTVL` = 5, `TCP_KEEPCNT` = 6, which is the guest's own numbering
/// — the guest under this runtime is an Android arm64 binary — so on a Linux host the adapter's
/// number and the backend's number happen to agree. **They must not be allowed to short-circuit
/// into a pass-through.** Windows numbers the same three 3, 17 and 16 and puts `TCP_MAXRT` on 5,
/// so a pass-through is silently wrong there; a seam that had one path on Linux and another on
/// Windows would be two implementations of one contract, and the one that is exercised least is
/// the one that would rot. See [`SocketOption::KeepAliveIdle`](super::SocketOption::KeepAliveIdle).
///
/// macOS is a third scheme again and is worth knowing before assuming BSD and Linux agree:
/// `TCP_KEEPALIVE` = 0x10 is the idle time, `TCP_KEEPINTVL` = 0x101 and `TCP_KEEPCNT` = 0x102.
/// The idle time there is in **seconds** like the others, which is the part most easily got
/// wrong, because the `SIO_KEEPALIVE_VALS` struct Windows offers as the *other* way to set these
/// is in milliseconds.
///
/// The value arrives here as whole seconds, already range-checked by
/// [`Socket::set_option`](super::Socket::set_option); this backend owes only the `setsockopt`.
pub(super) fn set_keep_alive_idle(inner: &Inner, seconds: u32) -> NetResult<()> {
    let _ = (inner, seconds);
    unsupported("setsockopt", "setsockopt(2) with IPPROTO_TCP/TCP_KEEPIDLE")
}

/// Intended: `getsockopt(IPPROTO_TCP, TCP_KEEPIDLE)`. See [`set_keep_alive_idle`].
pub(super) fn keep_alive_idle(inner: &Inner) -> NetResult<u32> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with IPPROTO_TCP/TCP_KEEPIDLE")
}

/// Intended: `setsockopt(IPPROTO_TCP, TCP_KEEPINTVL)`. See [`set_keep_alive_idle`].
pub(super) fn set_keep_alive_interval(inner: &Inner, seconds: u32) -> NetResult<()> {
    let _ = (inner, seconds);
    unsupported("setsockopt", "setsockopt(2) with IPPROTO_TCP/TCP_KEEPINTVL")
}

/// Intended: `getsockopt(IPPROTO_TCP, TCP_KEEPINTVL)`. See [`set_keep_alive_idle`].
pub(super) fn keep_alive_interval(inner: &Inner) -> NetResult<u32> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with IPPROTO_TCP/TCP_KEEPINTVL")
}

/// Intended: `setsockopt(IPPROTO_TCP, TCP_KEEPCNT)`. See [`set_keep_alive_idle`].
pub(super) fn set_keep_alive_count(inner: &Inner, count: u32) -> NetResult<()> {
    let _ = (inner, count);
    unsupported("setsockopt", "setsockopt(2) with IPPROTO_TCP/TCP_KEEPCNT")
}

/// Intended: `getsockopt(IPPROTO_TCP, TCP_KEEPCNT)`. See [`set_keep_alive_idle`].
pub(super) fn keep_alive_count(inner: &Inner) -> NetResult<u32> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with IPPROTO_TCP/TCP_KEEPCNT")
}

/// Intended: `getsockopt(SOL_SOCKET, SO_REUSEADDR)`.
pub(super) fn reuse_address(inner: &Inner) -> NetResult<bool> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with SOL_SOCKET/SO_REUSEADDR")
}

/// Intended: `setsockopt(SOL_SOCKET, SO_REUSEADDR)`.
///
/// The note worth carrying across: **it does not mean what it means on Windows.** Here it lifts
/// the `TIME_WAIT` restriction; there it lets a second socket take an address another one is
/// actively using. The seam sets what the host offers under the name and says so rather than
/// pretending the two agree.
pub(super) fn set_reuse_address(inner: &Inner, on: bool) -> NetResult<()> {
    let _ = (inner, on);
    unsupported("setsockopt", "setsockopt(2) with SOL_SOCKET/SO_REUSEADDR")
}

/// Intended: `getsockopt(SOL_SOCKET, SO_RCVBUF | SO_SNDBUF)`.
///
/// The decision in it: **Linux reports double what was set** and macOS does not, so neither
/// number is "the" answer and a caller must not be told it got what it asked for.
pub(super) fn buffer_bytes(inner: &Inner, which: Buffer) -> NetResult<usize> {
    let _ = inner;
    unsupported("getsockopt", match which {
        Buffer::Receive => "getsockopt(2) with SOL_SOCKET/SO_RCVBUF",
        Buffer::Send => "getsockopt(2) with SOL_SOCKET/SO_SNDBUF",
    })
}

/// Intended: `setsockopt(SOL_SOCKET, SO_RCVBUF | SO_SNDBUF)`.
///
/// The decision in it: **the kernel clamps to `net.core.rmem_max`/`wmem_max` on Linux and reports
/// success**, so a value larger than the clamp is not an error and not honoured. The refusal for
/// a size that does not fit an `int` belongs above this call, where it already is.
pub(super) fn set_buffer_bytes(inner: &Inner, which: Buffer, bytes: usize) -> NetResult<()> {
    let _ = (inner, bytes);
    unsupported("setsockopt", match which {
        Buffer::Receive => "setsockopt(2) with SOL_SOCKET/SO_RCVBUF",
        Buffer::Send => "setsockopt(2) with SOL_SOCKET/SO_SNDBUF",
    })
}

/// Intended: `getsockopt(IPPROTO_IPV6, IPV6_V6ONLY)`.
pub(super) fn v6only(inner: &Inner) -> NetResult<bool> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with IPPROTO_IPV6/IPV6_V6ONLY")
}

/// Intended: `setsockopt(IPPROTO_IPV6, IPV6_V6ONLY)`.
///
/// The decision in it: **the default differs by host.** Linux takes it from
/// `net.ipv6.bindv6only`, which is 0 on nearly every distribution, and macOS defaults it to 1. A
/// runtime that relies on a v6 socket also carrying v4 traffic must set it rather than assume it,
/// which is why this option is on the seam at all.
pub(super) fn set_v6only(inner: &Inner, on: bool) -> NetResult<()> {
    let _ = (inner, on);
    unsupported("setsockopt", "setsockopt(2) with IPPROTO_IPV6/IPV6_V6ONLY")
}

/// Intended: `setsockopt(SOL_SOCKET, SO_LINGER)` with `struct linger { on, 0 }`.
pub(super) fn set_linger(inner: &Inner, on: bool) -> NetResult<()> {
    let _ = (inner, on);
    unsupported("setsockopt", "setsockopt(2) with SOL_SOCKET/SO_LINGER")
}

/// Intended: `getsockopt(SOL_SOCKET, SO_LINGER)`.
pub(super) fn linger(inner: &Inner) -> NetResult<Option<u16>> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with SOL_SOCKET/SO_LINGER")
}

/// Intended: `getsockopt(SOL_SOCKET, SO_BROADCAST)`.
pub(super) fn broadcast(inner: &Inner) -> NetResult<bool> {
    let _ = inner;
    unsupported("getsockopt", "getsockopt(2) with SOL_SOCKET/SO_BROADCAST")
}

/// Intended: `setsockopt(SOL_SOCKET, SO_BROADCAST)`.
pub(super) fn set_broadcast(inner: &Inner, on: bool) -> NetResult<()> {
    let _ = (inner, on);
    unsupported("setsockopt", "setsockopt(2) with SOL_SOCKET/SO_BROADCAST")
}

/// Intended: `setsockopt(2)` with `IP_MTU_DISCOVER`/`IPV6_MTU_DISCOVER` = `IP_PMTUDISC_DONT`,
/// `_DO` or `_PROBE` (0, 2, 3 -- Linux's numbers), by family.
pub(super) fn set_path_mtu(inner: &Inner, family: IpFamily, mode: PathMtu) -> NetResult<()> {
    let _ = (inner, family, mode);
    unsupported("setsockopt", "setsockopt(2) with IP_MTU_DISCOVER/IPV6_MTU_DISCOVER")
}

/// Intended: `getsockopt(2)` with `IP_MTU_DISCOVER`/`IPV6_MTU_DISCOVER`, by family. Linux has no
/// "not set": a fresh socket reads `IP_PMTUDISC_WANT` (1), which is none of the three [`PathMtu`]
/// modes and would answer `None`.
pub(super) fn path_mtu(inner: &Inner, family: IpFamily) -> NetResult<Option<PathMtu>> {
    let _ = (inner, family);
    unsupported("getsockopt", "getsockopt(2) with IP_MTU_DISCOVER/IPV6_MTU_DISCOVER")
}

/// Intended: `poll(2)`.
///
/// The decisions in it, and the first is a change to what the seam *can* say:
///
/// * **`POLLHUP` is available here and is not on Windows.** `select` cannot report a peer that
///   closed, so the Windows backend never sets [`Readiness`](super::Readiness)'s `hangup` and
///   documents that end of file arrives as a readable socket and a zero-length `recv`. `poll(2)`
///   can do better, and should: flattening it to match Windows would discard information this
///   target has.
/// * **`POLLNVAL` means the descriptor is not open**, which is a caller error rather than a
///   socket state, and is the one condition that should become an error return rather than a
///   readiness bit.
/// * **There is no `FD_SETSIZE` here**, so [`MAX_POLL_SOCKETS`](super::MAX_POLL_SOCKETS)'s
///   refusal has no reason to fire on this target. Whoever implements this decides whether the
///   limit stays as one number for all five targets — which is simpler and refuses a call these
///   targets could serve — or becomes per-backend, which is honest and means a guest behaves
///   differently on two hosts. Neither is obviously right and the choice belongs to whoever can
///   run it.
pub(super) fn poll(entries: &mut [PollEntry<'_>], timeout: Duration) -> NetResult<usize> {
    let _ = (entries, timeout);
    unsupported("poll", "poll(2)")
}
