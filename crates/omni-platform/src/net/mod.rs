//! Sockets and name resolution: the platform seam for the network.
//!
//! **The only place in the workspace where a socket call is made** (Global Constraint 4). D30
//! withdrew Global Constraint 8 — "no network access at runtime" — because playable Roblox needs
//! login, settings and a game server, and this module is what the withdrawal required. It serves
//! the guest's `socket`, `connect`, `bind`, `send`/`recv`, `sendto`/`recvfrom`, `shutdown`,
//! `setsockopt`/`getsockopt`, `poll`/`select` over sockets, and `getaddrinfo`.
//!
//! # The policy is the first thing, not a feature
//!
//! A [`Socket`] **is** a descriptor plus a [`NetPolicy`], and there is no constructor that does
//! not take one — exactly as a [`Filesystem`](crate::fs::Filesystem) is a root plus a descriptor
//! table. The default policy is [`NetPolicy::closed`], which reaches nothing.
//!
//! That shape is the point of D30 rather than a decoration on it. The old constraint was reached
//! by a runtime that could not yet do anything a network was for; it was withdrawn by one that
//! can. But D6's threat is unchanged — **the APK under test is cheat-injected and carries a Luau
//! executor** — so what replaces a refusal is not an open socket. It is a question an embedding
//! answers: *which network may this instance reach.* See [`NetPolicy`] for the three gates, and for
//! the paragraph about what they cannot prevent, which is the part worth reading before trusting
//! them.
//!
//! # Where `std` serves five targets and where it does not: D23's test, answered "partly"
//!
//! D30 predicted this answer and asked for it to be said out loud. Here it is, operation by
//! operation, because the honest statement is sharper than the prediction was:
//!
//! | operation | how | Linux / macOS |
//! |---|---|---|
//! | [`send`](Socket::send), [`recv`](Socket::recv), [`send_to`](Socket::send_to), [`recv_from`](Socket::recv_from) | `std::net`, one call | **implemented** — portable `std`, no backend |
//! | [`shutdown`](Socket::shutdown), [`local_address`](Socket::local_address), [`peer_address`](Socket::peer_address) | `std::net`, one call | **implemented** — portable `std` |
//! | [`set_nonblocking`](Socket::set_nonblocking) | `std::net`'s `set_nonblocking` | **implemented** — portable `std` |
//! | `TCP_NODELAY`, `SO_RCVTIMEO`, `SO_SNDTIMEO` | `set_nodelay`, `set_read_timeout`, `set_write_timeout` | **implemented** — portable `std` |
//! | [`resolve`] | `std::net::ToSocketAddrs` | **implemented** — portable `std`; see [`resolve`] for the one gap in its *classification* |
//! | [`Socket::new`] | **backend**: `socket(2)` | **`Unsupported`**, naming `socket(2)` |
//! | [`connect`](Socket::connect), [`bind`](Socket::bind) | **backend**: `connect(2)`, `bind(2)` | **`Unsupported`** |
//! | `SO_ERROR`, `SO_REUSEADDR`, `SO_KEEPALIVE`, `SO_RCVBUF`, `SO_SNDBUF`, `IPV6_V6ONLY` | **backend**: `getsockopt`/`setsockopt` | **`Unsupported`** |
//! | the three keep-alive *timing* options | **backend**: `getsockopt`/`setsockopt`, **and the option numbers differ between hosts** — see [`SocketOption::KeepAliveIdle`] | **`Unsupported`** |
//! | [`poll`] — readiness | **backend**: `select` on Windows | **`Unsupported`**, naming `poll(2)` |
//!
//! **The one-sentence version, and it corrects the prediction in D30's own text.** That record
//! says "`std::net` carries connect/send/recv/non-blocking across all five targets", and the first
//! of those is wrong: `std` has **no way to make a socket that is not already connected or
//! bound**. `TcpStream::connect` blocks, `connect_timeout` refuses a zero duration, and there is
//! no `TcpStream::new`. So *creating* a socket, *starting* a connect and *binding* one are the
//! backend; everything you can do to a socket once you hold it is `std::net` and is written once
//! for all five targets. Readiness is the backend too, and that half of the prediction was right —
//! `poll`/`select`/`epoll` are not in `std` at all.
//!
//! A non-blocking connect is the case that matters and it is why the split falls where it does:
//! the engine will not park a thread on a connect, so "in progress, come back when it is
//! writable" is the *normal* path here and not an edge of one.
//!
//! **Nothing here has been run on Linux or macOS.** The portable half is expected to work there
//! and has not been built, let alone tested; the backend half is not written at all and says so by
//! name. That is the standing position for all five targets and it is not weakened by an
//! implementation existing.
//!
//! # Sockets belong in `fs`'s descriptor table, not in one of their own
//!
//! D30 requires it and [`pipe`](crate::fs::pipe) already gives the argument: `poll`, `select`,
//! `close` and `fcntl` observe **one** descriptor space, and two allocators handing out the same
//! number is not a wrong answer until the day a guest closes the wrong one. This module therefore
//! hands out a [`Socket`] and **no descriptor number**; the number comes from
//! [`Filesystem`](crate::fs::Filesystem), which is also what makes instance isolation free —
//! per-`Filesystem` means per-`Bionic` instance, exactly as descriptors already are.
//!
//! [`Socket::readiness`] answers in [`Readiness`], which is [`fs`](crate::fs)'s type and not a
//! second one, so that a socket entry slots into `Entry::readiness`'s `match` — the one with no
//! default arm — without anything having to translate between two vocabularies.
//!
//! # Nothing here waits longer than its caller said
//!
//! [`poll`] takes a [`Duration`] and there is no overload that does not. `poll(fds, n, -1)` is not
//! expressible through this seam, for the reason [`pipe`](crate::fs::pipe) gives for the same
//! decision: D16's runaway-guest defence is built from step budgets that a sleeping thread does
//! not consume, so "how long may a guest block" is a policy the adapter owns and not one a
//! platform seam should answer.
//!
//! # What is deliberately not here, and what a caller gets instead
//!
//! `libroblox.so` imports about thirty network symbols. **Importing is not calling** (D17), and
//! the rule that has held all session holds here: a primitive is built when a run has reached it,
//! and `Bionic::guest_thread_failures()` now names anything missing on the first run that hits it.
//! So this seam is a TCP and UDP **client** and nothing else:
//!
//! * **No `listen`, `accept`, `accept4` or `socketpair`.** Nothing here can receive an incoming
//!   connection. A caller gets no function to call: the refusal is the absence of a name rather
//!   than a stub that fails, and the adapter above refuses the guest symbol by name.
//! * **No `sendmsg`, `recvmsg`, `sendmmsg`, `recvmmsg`.** Scatter/gather and control messages have
//!   no portable `std` spelling and nothing has reached them. A guest that needs one is a guest
//!   whose message the adapter must refuse by name — and the day that happens, the missing work is
//!   a `WSASendMsg`/`sendmsg(2)` pair in the backend.
//! * **No `epoll_create`, `epoll_ctl`, `epoll_wait`.** [`poll`] is the readiness call; `epoll` is
//!   a Linux-only interface whose *state* lives in the kernel, which is a second descriptor kind
//!   and a second allocator, and D30 point 2 is explicit that there is one descriptor space.
//! * **No TLS, no certificates, no cryptography of any kind.** `libroblox.so` carries its own
//!   OpenSSL, measured — this layer owes it sockets and names and not one line more. A TLS
//!   implementation here would be a second one, disagreeing with the guest's about something.
//! * **No `if_nametoindex`/`if_indextoname`.** An interface *name* table is the host's and the
//!   guest's numbering would have to match it; nothing has reached them.
//!
//! An option outside [`SocketOption`] is the one case that is a refusal rather than an absence,
//! because the guest passes it as a *number* and the adapter cannot refuse a number it does not
//! recognise without help. [`NetError::unimplemented_option`] is that help, and it exists because
//! a `setsockopt` that is silently accepted is precisely the defect "no plausible stubs" is about.

mod address;
mod error;
mod policy;
pub mod record;
mod resolver;

pub use address::{IpFamily, SocketAddress};
pub use error::{NetError, NetErrorKind, NetResult, ResolveFailure};
pub use policy::NetPolicy;
pub use resolver::{resolve, service_port};

/// What a descriptor would do right now — **[`fs`](crate::fs)'s type, re-exported, not a second
/// one.**
///
/// A socket is a descriptor in the same table a pipe and a file are in (D30 point 2), and
/// `Entry::readiness` is a `match` with no default arm. Answering in a vocabulary of its own would
/// mean a translation at that `match`, and a translation is a place two meanings of "readable" can
/// drift apart.
pub use crate::fs::Readiness;

// The backend modules are **private**, for the reason `vm::mod` records: a `pub mod windows` is a
// public surface no other crate can name without writing `#[cfg(target_os = "windows")]` itself,
// which Global Constraint 4 forbids everywhere but here.
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as backend;

#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

/// How many sockets one [`poll`] call may name.
///
/// **A limit of the Windows readiness backend, stated rather than hidden.** `select` there takes
/// an `FD_SET` whose array is `FD_SETSIZE` entries long, and `FD_SETSIZE` is 64. A set larger than
/// this is **refused by name** rather than silently truncated, because a truncated poll answers
/// "nothing is ready" about descriptors it never looked at — which is a wrong answer a caller
/// cannot distinguish from a timeout.
///
/// `WSAPoll` would lift the limit and is deliberately not used; see [`poll`] for the measured
/// reason, which is a documented defect in `WSAPoll` rather than a preference.
pub const MAX_POLL_SOCKETS: usize = 64;

/// The socket options this seam implements, for the message an unimplemented one produces.
///
/// A single string rather than a list, because its only consumer is
/// [`NetError::unimplemented_option`], whose whole job is to tell the next person what *is*
/// available at the moment they discover what is not.
pub const IMPLEMENTED_OPTIONS: &str =
    "SO_ERROR (read-only), SO_REUSEADDR, SO_KEEPALIVE, TCP_NODELAY, SO_RCVBUF, SO_SNDBUF,      SO_RCVTIMEO, \
     SO_SNDTIMEO, IPV6_V6ONLY, TCP_KEEPIDLE (Windows spells it TCP_KEEPALIVE), TCP_KEEPINTVL, \
     TCP_KEEPCNT";

/// Which protocol a socket speaks.
///
/// Two, because two is what a client needs: TCP for HTTPS — settings, auth and API — and UDP for
/// the game protocol. `SOCK_RAW` and `SOCK_SEQPACKET` are not here and nothing has asked for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SocketKind {
    /// `SOCK_STREAM`: TCP.
    Stream,
    /// `SOCK_DGRAM`: UDP.
    Datagram,
}

impl SocketKind {
    /// Every variant, in order. Exists so that invariants can be asserted over all of them.
    pub const ALL: [SocketKind; 2] = [SocketKind::Stream, SocketKind::Datagram];

    /// A short name, for a message that has to say which kind refused.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SocketKind::Stream => "a stream socket (SOCK_STREAM/TCP)",
            SocketKind::Datagram => "a datagram socket (SOCK_DGRAM/UDP)",
        }
    }
}

/// Which directions of a stream to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shutdown {
    /// `SHUT_RD`: further receives return end of file.
    Read,
    /// `SHUT_WR`: the peer sees end of file. A FIN goes out.
    Write,
    /// `SHUT_RDWR`: both.
    Both,
}

impl Shutdown {
    /// `std`'s spelling of the same thing.
    const fn to_std(self) -> std::net::Shutdown {
        match self {
            Shutdown::Read => std::net::Shutdown::Read,
            Shutdown::Write => std::net::Shutdown::Write,
            Shutdown::Both => std::net::Shutdown::Both,
        }
    }
}

/// What a [`Socket::connect`] did.
///
/// **[`InProgress`](Self::InProgress) is the normal answer, not the exception.** A non-blocking
/// `connect` to anything off this machine returns immediately with the handshake still in flight;
/// the caller then waits for writability and asks [`Socket::connect_result`]. A `Connected` from a
/// real network would mean the whole three-way handshake completed inside one syscall, which
/// happens on loopback and essentially nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectProgress {
    /// The connection is established already — loopback, or a datagram socket, where `connect`
    /// only records a default peer and sends nothing.
    Connected,
    /// The handshake has started. `EINPROGRESS`.
    InProgress,
}

/// How a started connection turned out: the answer `SO_ERROR` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectOutcome {
    /// [`Socket::connect`] has not been called on this socket.
    NotStarted,
    /// The handshake is still in flight: the socket is neither writable nor in error.
    InProgress,
    /// The connection is established.
    Connected,
    /// The connection attempt failed, with the reason the stack recorded.
    Failed(NetErrorKind),
}

/// What a caller wants to hear about, in a [`poll`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interest {
    /// Tell me when a receive would return without blocking — including one that returns zero.
    pub readable: bool,
    /// Tell me when a send would take at least one byte, or when a pending connect has settled.
    pub writable: bool,
}

impl Interest {
    /// Readable only.
    pub const READABLE: Interest = Interest { readable: true, writable: false };
    /// Writable only. **What a pending connect is waited on with.**
    pub const WRITABLE: Interest = Interest { readable: false, writable: true };
    /// Both.
    pub const BOTH: Interest = Interest { readable: true, writable: true };
}

/// The empty readiness: nothing is ready and nothing is wrong.
///
/// [`Readiness`] has a constant for *always ready* and none for *not ready*, because before
/// sockets existed no descriptor in this runtime was ever the second thing for longer than a
/// function call. Spelled once here rather than at each of the four places that need it.
const NOT_READY: Readiness =
    Readiness { readable: false, writable: false, hangup: false, error: false };

/// One socket's place in a [`poll`] call: what is being watched, and what came back.
///
/// The three parallel slices this could have been — sockets, interests, results — are one struct
/// instead, because a caller that gets them out of step produces an answer about the wrong socket
/// and nothing can detect it.
#[derive(Debug)]
pub struct PollEntry<'a> {
    socket: &'a Socket,
    interest: Interest,
    readiness: Readiness,
}

impl<'a> PollEntry<'a> {
    /// Watch `socket` for `interest`.
    #[must_use]
    pub fn new(socket: &'a Socket, interest: Interest) -> PollEntry<'a> {
        PollEntry { socket, interest, readiness: NOT_READY }
    }

    /// The socket this entry watches.
    #[must_use]
    pub fn socket(&self) -> &Socket {
        self.socket
    }

    /// What was asked about.
    #[must_use]
    pub fn interest(&self) -> Interest {
        self.interest
    }

    /// What came back from the last [`poll`] this entry was passed to.
    ///
    /// Nothing set, before the first one — which is the truth rather than a default: an entry
    /// that has not been polled has not been told anything.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        self.readiness
    }

}

/// An option to set on a socket.
///
/// **A closed enum, and that is the enforcement.** Rule 1 of this project is that nothing may fake
/// success, and a `setsockopt` that is silently ignored is its exact shape: the caller believes
/// the option took effect and behaves as though it had. There is no `Raw(level, name, bytes)`
/// variant, so an option nobody has implemented cannot be routed through here at all — it reaches
/// the guest as [`NetError::unimplemented_option`], naming the numbers it passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SocketOption {
    /// `SO_REUSEADDR`.
    ///
    /// **It does not mean the same thing on Windows as it does on Linux**, and the difference is
    /// worth knowing before relying on it: on Windows it permits a second socket to bind an
    /// address another socket is *actively* using, which is a security problem Microsoft
    /// documents and answers with `SO_EXCLUSIVEADDRUSE`. On Linux it only lifts the `TIME_WAIT`
    /// restriction. This seam sets the option the host offers under that name and does not
    /// pretend the semantics match.
    ReuseAddress(bool),
    /// `SO_KEEPALIVE`: let the stack probe an idle connection and tear it down if the peer is gone.
    ///
    /// **MEASURED, and it is why this variant exists at all.** Roblox's own HTTP stack sets it on
    /// the settings socket, and with the option absent the seam refused by name -- correctly -- and
    /// killed the fetch thread, which was the last thing standing between the run and graphics.
    ///
    /// Backend work on every target, because **`std::net` has no spelling for it**: there is no
    /// `TcpStream::set_keepalive`. It was removed from `std` in 2018 precisely because the *timing*
    /// knobs (`TCP_KEEPIDLE`/`TCP_KEEPINTVL`/`TCP_KEEPCNT`, and Windows' `SIO_KEEPALIVE_VALS`) have
    /// no portable spelling -- which is a warning about this option's semantics, not just its API.
    /// This seam sets the **boolean** and nothing else. The idle interval is the host's default and
    /// differs between targets (two hours on both Linux and Windows by default, but tunable
    /// system-wide on each), so a caller that needs a particular timeout does not have one here and
    /// gets no pretence that it does.
    KeepAlive(bool),
    /// How long a connection may be idle before the first keep-alive probe is sent.
    ///
    /// Linux calls it `TCP_KEEPIDLE`; **Windows calls it `TCP_KEEPALIVE`**, which is a different
    /// name for the same quantity and is the reason this seam takes a [`Duration`] rather than
    /// passing an option number through. See the section below, which covers all three of the
    /// timing options and is the one thing worth reading before touching any of them.
    ///
    /// # Two numbering schemes that overlap wrongly, which is why this is an enum and not a number
    ///
    /// The three knobs exist on both hosts, mean the same thing on both, and are **counted in
    /// seconds on both**. Only their option numbers differ, and they differ in the worst possible
    /// way — the sets overlap, so a number passed straight through lands on a real option with a
    /// different meaning and the host reports success:
    ///
    /// | quantity | Linux (`netinet/tcp.h`, the guest's numbering) | Windows (`windows_sys::Win32::Networking::WinSock`) |
    /// |---|---|---|
    /// | idle before the first probe | `TCP_KEEPIDLE` = 4 | `TCP_KEEPALIVE` = 3 |
    /// | interval between probes | `TCP_KEEPINTVL` = 5 | `TCP_KEEPINTVL` = 17 |
    /// | probes before giving up | `TCP_KEEPCNT` = 6 | `TCP_KEEPCNT` = 16 |
    ///
    /// **The trap, stated because it is exactly the failure mode rule 1 exists for.** Windows
    /// defines `TCP_MAXRT` = 5 and `TCP_MAXRTMS` = 14 at the same level. Linux's
    /// `TCP_KEEPINTVL` is 5. So a pass-through of the guest's 5 would set the *maximum
    /// retransmit time* — a real option, accepted, returning zero — and the caller would believe
    /// it had configured a probe interval. Nothing would report anything. That is why this enum
    /// carries the quantity and the backend owns the number: there is no path from a guest
    /// integer to a host `setsockopt` that does not pass through a named variant.
    ///
    /// **Checked against `windows_sys` 0.61.2**, `Win32/Networking/WinSock/mod.rs`, which is the
    /// crate this backend actually calls: `TCP_KEEPALIVE: i32 = 3`, `TCP_KEEPCNT: i32 = 16`,
    /// `TCP_KEEPINTVL: i32 = 17`, `TCP_MAXRT: i32 = 5`, `TCP_NODELAY: i32 = 1`. What would
    /// falsify the mapping: a round trip that sets one of the three and reads back a *different*
    /// one, which is what this crate's loopback tests assert — each option is set to a distinct
    /// value and all three are read back, so a pair that had been swapped could not pass.
    ///
    /// # The units, and the one Windows version fact this depends on
    ///
    /// `TCP_KEEPALIVE` and `TCP_KEEPINTVL` take **seconds** on Windows, as they do on Linux, from
    /// Windows 10 1709 onward (`TCP_KEEPCNT` from 1703). Below those builds the options do not
    /// exist and `setsockopt` answers `WSAENOPROTOOPT`, which this seam reports as a host error
    /// naming the option rather than swallowing — an old host is then visibly an old host, not a
    /// silently unconfigured socket. Windows x86-64 is the only tested target (D30 point 5) and
    /// this one is 10.0.26200.
    ///
    /// A `Duration` that is not a whole number of seconds is **refused by name**: neither host
    /// can express it, and rounding it would set an interval the caller did not ask for. The
    /// guest side never produces one — `setsockopt` there carries an `int` of seconds — so the
    /// refusal is reachable only from this crate's own API, and its test constructs it.
    KeepAliveIdle(Duration),
    /// How long to wait between keep-alive probes once the first has gone unanswered.
    ///
    /// Linux `TCP_KEEPINTVL` = 5, Windows `TCP_KEEPINTVL` = 17 — the same name and a different
    /// number, which is the pairing most likely to be assumed equal. See
    /// [`KeepAliveIdle`](Self::KeepAliveIdle) for the table and for what 5 means on Windows.
    KeepAliveInterval(Duration),
    /// How many unanswered probes before the connection is declared dead.
    ///
    /// A count, not a time: Linux `TCP_KEEPCNT` = 6, Windows `TCP_KEEPCNT` = 16.
    KeepAliveCount(u32),
    /// `TCP_NODELAY`: send small writes immediately rather than coalescing them (Nagle off).
    ///
    /// A game client sets it. Meaningless on a datagram socket, where it is **refused by name**
    /// rather than accepted — a device answers `ENOPROTOOPT` there and so does this.
    NoDelay(bool),
    /// `SO_RCVBUF`, in bytes.
    ///
    /// **What the kernel does with the number is not what you asked for**, on every target: Linux
    /// doubles it for bookkeeping and clamps it to `net.core.rmem_max`, and Windows rounds. A
    /// caller that sets it and reads it back gets the kernel's answer, and
    /// [`SocketQuery::ReceiveBuffer`] returns that rather than the request — which is why the
    /// round-trip test in this crate asserts *the kernel agreed to something*, not equality.
    ReceiveBuffer(usize),
    /// `SO_SNDBUF`, in bytes. See [`ReceiveBuffer`](Self::ReceiveBuffer) for what the kernel does
    /// with the number.
    SendBuffer(usize),
    /// `SO_RCVTIMEO`. `None` is "no timeout", which is what a zero `timeval` means on a device.
    ///
    /// **It has no effect on a non-blocking socket**, on any target, because a non-blocking
    /// receive never waits and so never times out. That is the host's behaviour rather than this
    /// seam's, and it is stated because a caller that sets both and expects the timeout to bound
    /// something is expecting something no operating system does.
    ReceiveTimeout(Option<Duration>),
    /// `SO_SNDTIMEO`. See [`ReceiveTimeout`](Self::ReceiveTimeout).
    SendTimeout(Option<Duration>),
    /// `IPV6_V6ONLY`: whether an IPv6 socket also carries IPv4 traffic as mapped addresses.
    ///
    /// Refused by name on an IPv4 socket, where the option does not exist.
    V6Only(bool),
}

/// An option to read back from a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SocketQuery {
    /// `SO_ERROR`: the pending socket error, which the host is entitled to **consume** as it is
    /// read.
    ///
    /// POSIX and Microsoft both document the read as clearing the error, and this seam is built
    /// so that a host doing so cannot lose it: [`Socket::connect_result`] moves whatever it read
    /// into the socket's own pending slot and leaves it there, so a guest's later
    /// `getsockopt(SO_ERROR)` still sees it rather than finding that a host-side caller had
    /// already taken it.
    ///
    /// **MEASURED on this host: it does not clear, for a refused connect.** Two consecutive
    /// `getsockopt(SOL_SOCKET, SO_ERROR)` calls on a socket whose connect was refused both
    /// returned `WSAECONNREFUSED`. That is Winsock departing from its own documentation, it is
    /// asserted in `net_loopback.rs` so that a host which starts clearing is noticed, and it is
    /// the reason this doc comment states the *contract* and the *measurement* separately: a
    /// caller must read the error once and keep it, because the host that clears is the one it
    /// has to be correct on.
    Error,
    /// `SO_REUSEADDR`.
    ReuseAddress,
    /// `SO_KEEPALIVE` -- the boolean only; see [`SocketOption::KeepAlive`] for what is not carried.
    KeepAlive,
    /// The keep-alive idle time: Linux `TCP_KEEPIDLE`, Windows `TCP_KEEPALIVE`. Answers
    /// [`OptionValue::Interval`].
    ///
    /// **This half exists because the set half cannot be verified without it.** An option number
    /// mapped to the wrong host constant is silently wrong — the `setsockopt` succeeds — so the
    /// only evidence that the mapping is right is reading the three back and finding the three
    /// values that went in. See [`SocketOption::KeepAliveIdle`] for the numbers and the trap.
    KeepAliveIdle,
    /// The keep-alive probe interval: `TCP_KEEPINTVL` on both, **different numbers**. Answers
    /// [`OptionValue::Interval`].
    KeepAliveInterval,
    /// The keep-alive probe count: `TCP_KEEPCNT` on both, **different numbers**. Answers
    /// [`OptionValue::Count`].
    KeepAliveCount,
    /// `TCP_NODELAY`.
    NoDelay,
    /// `SO_RCVBUF`, in bytes — the kernel's number, not the one that was asked for.
    ReceiveBuffer,
    /// `SO_SNDBUF`, in bytes — the kernel's number.
    SendBuffer,
    /// `SO_RCVTIMEO`.
    ReceiveTimeout,
    /// `SO_SNDTIMEO`.
    SendTimeout,
    /// `IPV6_V6ONLY`.
    V6Only,
}

/// What a [`SocketQuery`] answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OptionValue {
    /// The answer to [`SocketQuery::Error`]: `None` when there is no pending error.
    Error(Option<NetErrorKind>),
    /// A boolean option.
    Flag(bool),
    /// A size in bytes.
    Bytes(usize),
    /// A timeout, `None` meaning "no timeout".
    Timeout(Option<Duration>),
    /// An interval that is always present, which is what separates it from [`Timeout`](Self::Timeout).
    ///
    /// The keep-alive idle time and probe interval are of this shape: there is no "no interval"
    /// value for either — a socket always has one, defaulted by the host — and they are written
    /// into a guest `int` of seconds rather than a `struct timeval`. Reusing `Timeout` would have
    /// meant a `None` nothing can produce and a marshalling that is wrong by twelve bytes.
    Interval(Duration),
    /// A count of things, which is neither a size in bytes nor a duration.
    Count(u32),
}

/// Which of the two buffer options a backend call is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Buffer {
    /// `SO_RCVBUF`.
    Receive,
    /// `SO_SNDBUF`.
    Send,
}

impl Buffer {
    /// The name the refusal message uses.
    const fn as_str(self) -> &'static str {
        match self {
            Buffer::Receive => "SO_RCVBUF",
            Buffer::Send => "SO_SNDBUF",
        }
    }
}

/// The host socket under a [`Socket`].
///
/// **`std`'s types, deliberately**, even though the backend made the descriptor: once a socket
/// exists, everything done to it is one portable `std` call, and holding it as a `TcpStream` or a
/// `UdpSocket` is what makes that true without a `cfg` anywhere in this file. The backend reaches
/// through `AsRawSocket`/`AsRawFd` for the handful of calls `std` has no spelling for.
///
/// Both types close their descriptor on `Drop`, which is why there is no `Socket::close`: the
/// reference counting reason [`PipeHandle`](crate::fs::pipe::PipeHandle) gives applies unchanged —
/// a close that had to be remembered is a close that can be forgotten.
#[derive(Debug)]
enum Inner {
    /// `SOCK_STREAM`.
    Tcp(TcpStream),
    /// `SOCK_DGRAM`.
    Udp(UdpSocket),
}

/// Whether a connect has been asked for on a socket, and whether it has settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectState {
    /// No `connect` has been called.
    None,
    /// A `connect` has been called and has not been observed to settle.
    Started,
    /// A `connect` has been observed to settle, one way or the other.
    Settled,
}

/// One socket: a host descriptor, the policy it was created under, and the connect it has begun.
///
/// Created by [`Socket::new`], closed by dropping it. It carries **no descriptor number**: the
/// number is [`Filesystem`](crate::fs::Filesystem)'s, because `poll`, `select` and `close` observe
/// one descriptor space (D30 point 2), and instance isolation follows from that rather than from
/// anything here.
#[derive(Debug)]
pub struct Socket {
    inner: Inner,
    kind: SocketKind,
    family: IpFamily,
    nonblocking: bool,
    policy: Arc<NetPolicy>,
    connect: ConnectState,
    /// Whether anything has given this socket a local name: a successful `bind` or `connect`.
    ///
    /// **Consulted only when `getsockname` has already failed**, and it exists because of one
    /// measured difference between the hosts this seam targets. See
    /// [`local_address`](Socket::local_address).
    named: bool,
    /// The pending socket error, once something has taken it out of the kernel.
    ///
    /// **Modelled on the kernel's own slot rather than added to it.** `SO_ERROR` is consumed by
    /// reading it, so a host-side caller that asked [`Socket::connect_result`] would otherwise
    /// take the error the guest's own `getsockopt(SO_ERROR)` is about to ask for. Moving it here
    /// keeps the *count* right: the error is readable exactly once, by whoever asks first, which
    /// is what a device does.
    pending_error: Option<NetErrorKind>,
    /// This socket's identity in [`record`], handed out at creation whether or not anything is
    /// recording.
    ///
    /// **Assigned unconditionally, and that is deliberate.** A recording turned on after a socket
    /// exists still has to be able to name it, and an `Option` here would mean the identity
    /// depended on when the switch was thrown — so the same socket would appear under two names
    /// in two runs. The cost when nothing is recording is one relaxed increment per `socket(2)`.
    record: u64,
}

impl Socket {
    /// Create a socket: `socket(2)`.
    ///
    /// The socket starts **blocking**, as `socket(2)` does; call
    /// [`set_nonblocking`](Self::set_nonblocking) to change it. It is unbound and unconnected, so
    /// `getsockname` on it reports the wildcard address with port 0 — which is the truth about a
    /// fresh socket rather than a placeholder, and is why there is no lazy state here.
    ///
    /// # Errors
    ///
    /// [`NetError::Unsupported`] on Linux and macOS, naming `socket(2)`, and [`NetError::Io`] when
    /// the host refuses.
    pub fn new(kind: SocketKind, family: IpFamily, policy: Arc<NetPolicy>) -> NetResult<Socket> {
        let inner = match kind {
            SocketKind::Stream => Inner::Tcp(backend::create_stream(family)?),
            SocketKind::Datagram => Inner::Udp(backend::create_datagram(family)?),
        };
        Ok(Socket {
            inner,
            kind,
            family,
            nonblocking: false,
            policy,
            connect: ConnectState::None,
            named: false,
            pending_error: None,
            record: record::next_id(),
        })
    }

    /// Which protocol this socket speaks.
    #[must_use]
    pub fn kind(&self) -> SocketKind {
        self.kind
    }

    /// Which IP family this socket was created for.
    #[must_use]
    pub fn family(&self) -> IpFamily {
        self.family
    }

    /// The policy this socket was created under.
    ///
    /// Public so that an embedding can assert what it handed in, and so that a refusal seen in a
    /// log can be checked against the rules that produced it rather than guessed at.
    #[must_use]
    pub fn policy(&self) -> &Arc<NetPolicy> {
        &self.policy
    }

    /// Whether `O_NONBLOCK` is set.
    #[must_use]
    pub fn nonblocking(&self) -> bool {
        self.nonblocking
    }

    /// Set or clear non-blocking mode: `ioctl(FIONBIO)`, or `fcntl(F_SETFL, O_NONBLOCK)`.
    ///
    /// **A property of the socket rather than of a call**, which is what the guest's `fcntl` sets
    /// and what every send and receive here then answers to.
    ///
    /// # Errors
    ///
    /// [`NetError::Io`] when the host refuses.
    pub fn set_nonblocking(&mut self, nonblocking: bool) -> NetResult<()> {
        const OP: &str = "set_nonblocking";
        match &self.inner {
            Inner::Tcp(stream) => stream.set_nonblocking(nonblocking),
            Inner::Udp(socket) => socket.set_nonblocking(nonblocking),
        }
        .map_err(|error| NetError::io(OP, self.describe(), &error))?;
        self.nonblocking = nonblocking;
        Ok(())
    }

    /// Bind a local address: `bind(2)`.
    ///
    /// **Not policy-checked, and that is a decision rather than an omission.** [`NetPolicy`] is a
    /// *destination* policy — which network this instance may reach — and a local address is not a
    /// destination. What a bind does open is an inbound path: a datagram socket bound to the
    /// wildcard address receives from anyone who can route to this host. That is a different
    /// question from the one the policy answers, this seam has no `listen` and no `accept` so a
    /// stream socket cannot accept anything regardless, and the limit is written down here rather
    /// than left for somebody to assume the policy covers it.
    ///
    /// # Errors
    ///
    /// [`NetError::Unsupported`] on Linux and macOS, and [`NetError::Io`] — `EADDRINUSE` when
    /// something already holds the address, `EINVAL` when the socket is already bound.
    pub fn bind(&mut self, address: &SocketAddress) -> NetResult<()> {
        const OP: &str = "bind";
        self.require_family(OP, address)?;
        backend::bind(&self.inner, address)?;
        self.named = true;
        Ok(())
    }

    /// Start connecting: `connect(2)`.
    ///
    /// On a **stream** socket this is the non-blocking connect, and
    /// [`ConnectProgress::InProgress`] is the answer to expect for anything off this machine. The
    /// caller then waits for writability — [`poll`] with [`Interest::WRITABLE`] — and asks
    /// [`connect_result`](Self::connect_result), which is what reading `SO_ERROR` is for.
    ///
    /// On a **datagram** socket `connect` sends nothing: it records a default peer so that
    /// [`send`](Self::send) and [`recv`](Self::recv) work without an address, and filters incoming
    /// datagrams to that peer. It therefore always answers [`ConnectProgress::Connected`].
    ///
    /// # Errors
    ///
    /// * [`NetError::Policy`] when the embedding's policy does not admit the destination —
    ///   **before** any packet leaves this machine.
    /// * [`NetError::Io`] with [`NetErrorKind::AddressFamilyNotSupported`] when the address's
    ///   family is not the socket's. A device answers `EAFNOSUPPORT` and so does this, rather than
    ///   converting between families behind the caller's back.
    /// * [`NetError::Unsupported`] on Linux and macOS.
    pub fn connect(&mut self, address: &SocketAddress) -> NetResult<ConnectProgress> {
        const OP: &str = "connect";
        self.require_family(OP, address)?;
        self.policy.check_address(OP, address)?;
        let progress = match &self.inner {
            Inner::Tcp(_) => backend::start_connect(&self.inner, address)?,
            Inner::Udp(socket) => {
                socket
                    .connect(address.to_std())
                    .map_err(|error| NetError::io(OP, address.to_string(), &error))?;
                ConnectProgress::Connected
            }
        };
        self.connect = match progress {
            ConnectProgress::Connected => ConnectState::Settled,
            ConnectProgress::InProgress => ConnectState::Started,
        };
        // A connect assigns a local address, so `getsockname` has something to answer from here
        // on — see `local_address` for why this seam records that rather than asking.
        self.named = true;
        record::note_peer(self.record, &address.to_string());
        Ok(progress)
    }

    /// How the started connection turned out, the way `SO_ERROR` says it.
    ///
    /// The sequence this implements is the only correct one for a non-blocking connect, and it is
    /// two steps rather than one: **wait for the socket to become writable or to signal an
    /// exception, and only then read `SO_ERROR`.** A socket whose handshake is still in flight
    /// reads `SO_ERROR` as zero, so reading it alone cannot tell success from *not yet* — which is
    /// the plausible wrong answer this shape invites, and the one that turns a refused connection
    /// into a connection the caller believes it has.
    ///
    /// The error, once read, is moved into this socket's pending slot rather than discarded, so a
    /// later [`SocketQuery::Error`] still sees it exactly once. See this type's `pending_error` field.
    ///
    /// # Errors
    ///
    /// [`NetError::Unsupported`] on Linux and macOS, and [`NetError::Io`] when the readiness call
    /// or the `getsockopt` fails.
    pub fn connect_result(&mut self) -> NetResult<ConnectOutcome> {
        if self.connect == ConnectState::None {
            return Ok(ConnectOutcome::NotStarted);
        }
        if let Some(kind) = self.pending_error {
            return Ok(ConnectOutcome::Failed(kind));
        }
        let readiness = self.readiness()?;
        if !readiness.writable && !readiness.error {
            return Ok(ConnectOutcome::InProgress);
        }
        self.connect = ConnectState::Settled;
        match backend::socket_error(&self.inner)? {
            Some(kind) => {
                self.pending_error = Some(kind);
                Ok(ConnectOutcome::Failed(kind))
            }
            None if readiness.error => {
                // The readiness backend reported an exception and the stack has no error to go
                // with it. That is not a state this seam can describe, so it says so rather than
                // reporting a connection it has no evidence for.
                Err(NetError::refused(
                    "connect_result",
                    self.describe(),
                    "the socket signalled an exceptional condition and SO_ERROR read zero, so \
                     there is no failure to report and no evidence of success either. Reporting \
                     `Connected` here would be a guess about a socket the host has just said \
                     something is wrong with",
                ))
            }
            None => Ok(ConnectOutcome::Connected),
        }
    }

    /// The address this socket is bound to: `getsockname(2)`.
    ///
    /// The wildcard address with port 0 for a socket that has neither been bound nor sent
    /// anything, which is what POSIX says and what a device reports.
    ///
    /// # The one place this seam papers over a host difference, and why it is not a guess
    ///
    /// **MEASURED on this host: `getsockname` on a socket nothing has named fails with
    /// `WSAEINVAL` (10022).** POSIX requires the wildcard address and port 0; Winsock refuses the
    /// call outright, and both Microsoft and every BSD-derived stack are within their own
    /// documentation. `net_loopback.rs` carries the measurement as an assertion so that a host
    /// which changes its mind is noticed.
    ///
    /// Reporting that refusal upward would make the guest's `getsockname` answer `EINVAL` on one
    /// host and `0.0.0.0:0` on another for a socket in the same state, which is a behaviour
    /// difference a guest can see and nobody would find again. So the wildcard is returned — and
    /// it is a **fact this type holds**, not a substitute for one it could not obtain: `bind` and
    /// `connect` are the only operations that give a socket a name and both go through this type,
    /// so `named` being false means nothing has named it. Anything else `getsockname` fails with,
    /// and any failure at all once something *has* named the socket, is reported.
    ///
    /// The one path that names a socket without this type hearing about it is a kernel's
    /// auto-bind on the first `sendto`, and it cannot produce a wrong answer here: after an
    /// auto-bind `getsockname` succeeds, so the fallback is never reached.
    ///
    /// # Errors
    ///
    /// [`NetError::Io`] when the host refuses for any other reason.
    pub fn local_address(&self) -> NetResult<SocketAddress> {
        const OP: &str = "getsockname";
        let answer = match &self.inner {
            Inner::Tcp(stream) => stream.local_addr(),
            Inner::Udp(socket) => socket.local_addr(),
        };
        match answer {
            Ok(address) => Ok(SocketAddress::from_std(address)),
            Err(error)
                if !self.named
                    && NetErrorKind::classify(&error) == NetErrorKind::InvalidInput =>
            {
                Ok(SocketAddress::unspecified(self.family))
            }
            Err(error) => Err(NetError::io(OP, self.describe(), &error)),
        }
    }

    /// The address this socket is connected to: `getpeername(2)`.
    ///
    /// # Errors
    ///
    /// [`NetError::Io`] with [`NetErrorKind::NotConnected`] when there is no peer.
    pub fn peer_address(&self) -> NetResult<SocketAddress> {
        const OP: &str = "getpeername";
        match &self.inner {
            Inner::Tcp(stream) => stream.peer_addr(),
            Inner::Udp(socket) => socket.peer_addr(),
        }
        .map(SocketAddress::from_std)
        .map_err(|error| NetError::io(OP, self.describe(), &error))
    }

    /// Send on a connected socket: `send(2)`.
    ///
    /// **A short send is the contract, not a failure**, on a stream socket: the kernel takes what
    /// fits in its send buffer and reports how much that was, and a caller that ignores the count
    /// is wrong on a device too.
    ///
    /// # Errors
    ///
    /// [`NetError::Io`] with [`NetErrorKind::WouldBlock`] when a non-blocking socket's send buffer
    /// is full — distinctly from any other failure, which is the distinction a non-blocking client
    /// gets wrong. [`NetError::is_would_block`] asks the question without matching a shape.
    pub fn send(&self, buf: &[u8]) -> NetResult<usize> {
        const OP: &str = "send";
        match &self.inner {
            Inner::Tcp(stream) => {
                // `impl Write for &TcpStream`: a shared reference is all a send needs, which is
                // why every method on this type but the six that change it takes `&self`.
                let mut handle = stream;
                handle.write(buf)
            }
            Inner::Udp(socket) => socket.send(buf),
        }
        .inspect(|sent| record::note(self.record, record::Direction::Sent, &buf[..*sent]))
        .map_err(|error| NetError::io(OP, self.describe(), &error))
    }

    /// Receive on a connected socket: `recv(2)`.
    ///
    /// **`Ok(0)` on a stream socket means the peer closed**, and is not an error and not a
    /// would-block. It is the only way end of file is reported, and a caller that treats it as
    /// "nothing arrived yet" loops for ever on a connection that is over. On a datagram socket
    /// `Ok(0)` is a zero-length datagram, which is a legal thing to send.
    ///
    /// # Errors
    ///
    /// [`NetError::Io`] with [`NetErrorKind::WouldBlock`] when a non-blocking socket has nothing
    /// to give.
    pub fn recv(&self, buf: &mut [u8]) -> NetResult<usize> {
        const OP: &str = "recv";
        match &self.inner {
            Inner::Tcp(stream) => {
                let mut handle = stream;
                handle.read(buf)
            }
            Inner::Udp(socket) => socket.recv(buf),
        }
        .inspect(|read| record::note(self.record, record::Direction::Received, &buf[..*read]))
        .map_err(|error| NetError::io(OP, self.describe(), &error))
    }

    /// Send a datagram to an address: `sendto(2)`.
    ///
    /// **Policy-checked at every call**, not only at connect: a datagram carries its own
    /// destination, so a policy consulted only by [`connect`](Self::connect) would never see one.
    ///
    /// # Errors
    ///
    /// * [`NetError::Policy`] when the destination is outside the embedding's policy.
    /// * [`NetError::Refused`] on a stream socket. `sendto` with an address on a connected TCP
    ///   socket is `EISCONN` on Linux, and with a null address it is simply `send` — so the
    ///   caller wants [`send`](Self::send) and is told so by name rather than having its address
    ///   quietly ignored.
    /// * [`NetError::Io`] with [`NetErrorKind::MessageSize`] for a datagram larger than the path
    ///   will carry.
    pub fn send_to(&self, buf: &[u8], address: &SocketAddress) -> NetResult<usize> {
        const OP: &str = "sendto";
        let Inner::Udp(socket) = &self.inner else {
            return Err(NetError::refused(
                OP,
                self.describe(),
                "sendto with a destination address is not defined on a stream socket: a connected \
                 TCP socket answers EISCONN, and an unconnected one has nowhere to send. Use \
                 `send` on a stream socket — the address is refused rather than ignored, because \
                 ignoring it would send the bytes somewhere the caller did not ask for",
            ));
        };
        self.require_family(OP, address)?;
        self.policy.check_address(OP, address)?;
        socket
            .send_to(buf, address.to_std())
            .inspect(|sent| record::note(self.record, record::Direction::Sent, &buf[..*sent]))
            .map_err(|error| NetError::io(OP, address.to_string(), &error))
    }

    /// Receive a datagram and say where it came from: `recvfrom(2)`.
    ///
    /// **The datagram is truncated if it does not fit**, exactly as on a device, and this seam
    /// cannot report that it was: `recvfrom` returns the number of bytes *copied*, and the excess
    /// is discarded by the kernel before this layer sees anything. A caller that needs to know
    /// gives a buffer at least as large as the largest datagram it expects.
    ///
    /// # Errors
    ///
    /// * [`NetError::Refused`] on a stream socket, which has no per-message source address —
    ///   `recvfrom` there is `recv` with a null address, so the caller wants
    ///   [`recv`](Self::recv).
    /// * [`NetError::Io`] with [`NetErrorKind::WouldBlock`] when nothing has arrived.
    pub fn recv_from(&self, buf: &mut [u8]) -> NetResult<(usize, SocketAddress)> {
        const OP: &str = "recvfrom";
        let Inner::Udp(socket) = &self.inner else {
            return Err(NetError::refused(
                OP,
                self.describe(),
                "recvfrom on a stream socket has no source address to report: a stream is one \
                 conversation with one peer, and `recvfrom` there is `recv` with a null address. \
                 Use `recv`, and `peer_address` for who is on the other end",
            ));
        };
        socket
            .recv_from(buf)
            .inspect(|(read, _)| {
                record::note(self.record, record::Direction::Received, &buf[..*read]);
            })
            .map(|(read, from)| (read, SocketAddress::from_std(from)))
            .map_err(|error| NetError::io(OP, self.describe(), &error))
    }

    /// Close one or both directions of a stream: `shutdown(2)`.
    ///
    /// # Errors
    ///
    /// * [`NetError::Refused`] on a datagram socket. `shutdown` on a connected UDP socket is a
    ///   real thing on Linux and `std::net::UdpSocket` has no spelling for it, so implementing it
    ///   would mean a raw call in the backend for a case nothing has reached. It is refused by
    ///   name, naming what would implement it, rather than accepted and ignored.
    /// * [`NetError::Io`] with [`NetErrorKind::NotConnected`] on a stream with no peer.
    pub fn shutdown(&self, how: Shutdown) -> NetResult<()> {
        const OP: &str = "shutdown";
        let Inner::Tcp(stream) = &self.inner else {
            return Err(NetError::refused(
                OP,
                self.describe(),
                "shutdown is implemented for stream sockets only. On a connected datagram socket \
                 Linux does honour it, and `std::net::UdpSocket` has no shutdown at all, so \
                 implementing it means a raw `shutdown(2)` in this crate's per-OS backend — \
                 which nothing has yet reached and which is not written on speculation",
            ));
        };
        stream
            .shutdown(how.to_std())
            .map_err(|error| NetError::io(OP, self.describe(), &error))
    }

    /// Set a socket option: `setsockopt(2)`.
    ///
    /// # Errors
    ///
    /// * [`NetError::Refused`] for an option that exists and is not defined on **this** socket —
    ///   `TCP_NODELAY` on a datagram socket, `IPV6_V6ONLY` on an IPv4 one. A device answers
    ///   `ENOPROTOOPT`, and so does this rather than accepting it.
    /// * [`NetError::Unsupported`] on Linux and macOS for the four options that need a
    ///   `setsockopt` call.
    /// * [`NetError::Io`] when the host refuses the value.
    ///
    /// An option this seam does not implement at all cannot reach here: [`SocketOption`] is a
    /// closed enum, and the adapter refuses it with [`NetError::unimplemented_option`].
    pub fn set_option(&mut self, option: SocketOption) -> NetResult<()> {
        const OP: &str = "setsockopt";
        match option {
            SocketOption::ReuseAddress(on) => backend::set_reuse_address(&self.inner, on),
            SocketOption::KeepAlive(on) => backend::set_keep_alive(&self.inner, on),
            SocketOption::KeepAliveIdle(idle) => {
                self.require_stream(OP, KEEP_ALIVE_IDLE)?;
                let seconds = whole_seconds(OP, KEEP_ALIVE_IDLE, idle)?;
                backend::set_keep_alive_idle(&self.inner, seconds)
            }
            SocketOption::KeepAliveInterval(interval) => {
                self.require_stream(OP, KEEP_ALIVE_INTERVAL)?;
                let seconds = whole_seconds(OP, KEEP_ALIVE_INTERVAL, interval)?;
                backend::set_keep_alive_interval(&self.inner, seconds)
            }
            SocketOption::KeepAliveCount(count) => {
                self.require_stream(OP, KEEP_ALIVE_COUNT)?;
                backend::set_keep_alive_count(&self.inner, count)
            }
            SocketOption::NoDelay(on) => {
                let stream = self.require_stream(OP, "TCP_NODELAY")?;
                stream.set_nodelay(on).map_err(|e| NetError::io(OP, self.describe(), &e))
            }
            SocketOption::ReceiveBuffer(bytes) => {
                backend::set_buffer_bytes(&self.inner, Buffer::Receive, bytes)
            }
            SocketOption::SendBuffer(bytes) => {
                backend::set_buffer_bytes(&self.inner, Buffer::Send, bytes)
            }
            SocketOption::ReceiveTimeout(timeout) => {
                let timeout = normalise_timeout(timeout);
                match &self.inner {
                    Inner::Tcp(stream) => stream.set_read_timeout(timeout),
                    Inner::Udp(socket) => socket.set_read_timeout(timeout),
                }
                .map_err(|e| NetError::io(OP, self.describe(), &e))
            }
            SocketOption::SendTimeout(timeout) => {
                let timeout = normalise_timeout(timeout);
                match &self.inner {
                    Inner::Tcp(stream) => stream.set_write_timeout(timeout),
                    Inner::Udp(socket) => socket.set_write_timeout(timeout),
                }
                .map_err(|e| NetError::io(OP, self.describe(), &e))
            }
            SocketOption::V6Only(on) => {
                self.require_v6(OP)?;
                backend::set_v6only(&self.inner, on)
            }
        }
    }

    /// Read a socket option back: `getsockopt(2)`.
    ///
    /// **Takes `&mut self` because of one query.** [`SocketQuery::Error`] is `SO_ERROR`, which is
    /// consumed by reading it on every target; the read therefore changes the socket, and a
    /// signature that said otherwise would be a lie about the most surprising option in the set.
    ///
    /// # Errors
    ///
    /// As [`set_option`](Self::set_option).
    pub fn get_option(&mut self, query: SocketQuery) -> NetResult<OptionValue> {
        const OP: &str = "getsockopt";
        match query {
            SocketQuery::Error => {
                if let Some(kind) = self.pending_error.take() {
                    return Ok(OptionValue::Error(Some(kind)));
                }
                backend::socket_error(&self.inner).map(OptionValue::Error)
            }
            SocketQuery::ReuseAddress => {
                backend::reuse_address(&self.inner).map(OptionValue::Flag)
            }
            SocketQuery::KeepAlive => backend::keep_alive(&self.inner).map(OptionValue::Flag),
            SocketQuery::KeepAliveIdle => {
                self.require_stream(OP, KEEP_ALIVE_IDLE)?;
                backend::keep_alive_idle(&self.inner)
                    .map(|seconds| OptionValue::Interval(Duration::from_secs(u64::from(seconds))))
            }
            SocketQuery::KeepAliveInterval => {
                self.require_stream(OP, KEEP_ALIVE_INTERVAL)?;
                backend::keep_alive_interval(&self.inner)
                    .map(|seconds| OptionValue::Interval(Duration::from_secs(u64::from(seconds))))
            }
            SocketQuery::KeepAliveCount => {
                self.require_stream(OP, KEEP_ALIVE_COUNT)?;
                backend::keep_alive_count(&self.inner).map(OptionValue::Count)
            }
            SocketQuery::NoDelay => {
                let stream = self.require_stream(OP, "TCP_NODELAY")?;
                stream
                    .nodelay()
                    .map(OptionValue::Flag)
                    .map_err(|e| NetError::io(OP, self.describe(), &e))
            }
            SocketQuery::ReceiveBuffer => {
                backend::buffer_bytes(&self.inner, Buffer::Receive).map(OptionValue::Bytes)
            }
            SocketQuery::SendBuffer => {
                backend::buffer_bytes(&self.inner, Buffer::Send).map(OptionValue::Bytes)
            }
            SocketQuery::ReceiveTimeout => match &self.inner {
                Inner::Tcp(stream) => stream.read_timeout(),
                Inner::Udp(socket) => socket.read_timeout(),
            }
            .map(OptionValue::Timeout)
            .map_err(|e| NetError::io(OP, self.describe(), &e)),
            SocketQuery::SendTimeout => match &self.inner {
                Inner::Tcp(stream) => stream.write_timeout(),
                Inner::Udp(socket) => socket.write_timeout(),
            }
            .map(OptionValue::Timeout)
            .map_err(|e| NetError::io(OP, self.describe(), &e)),
            SocketQuery::V6Only => {
                self.require_v6(OP)?;
                backend::v6only(&self.inner).map(OptionValue::Flag)
            }
        }
    }

    /// What this socket would do right now, as `poll` and `select` ask it.
    ///
    /// **This is the call [`fs`](crate::fs) needs**, and it is the one `std` cannot make: there is
    /// no readiness in the standard library at all, which is why this module has a per-OS backend
    /// and why D25's "the day a phase binds `socket` for real, this module has to grow a real
    /// readiness source" has arrived.
    ///
    /// It is **fallible**, unlike every other kind's readiness in the descriptor table, and that
    /// is not a style difference: a pipe's readiness is two integers this process owns, and a
    /// socket's is a question for the operating system, which can refuse it. `Entry::readiness` is
    /// infallible today, so a socket variant has a decision to make about a refused poll —
    /// reporting `error: true` is the answer a device's `POLLNVAL` gives and is the only one that
    /// does not invent readiness a caller would act on.
    ///
    /// # Errors
    ///
    /// [`NetError::Unsupported`] on Linux and macOS, naming `poll(2)`, and [`NetError::Io`] when
    /// the host's readiness call fails.
    pub fn readiness(&self) -> NetResult<Readiness> {
        let mut entries = [PollEntry::new(self, Interest::BOTH)];
        poll(&mut entries, Duration::ZERO)?;
        Ok(entries[0].readiness())
    }

    /// The stream under this socket, or a refusal naming the option that is not defined on a
    /// datagram one.
    fn require_stream(&self, operation: &'static str, option: &'static str) -> NetResult<&TcpStream> {
        match &self.inner {
            Inner::Tcp(stream) => Ok(stream),
            Inner::Udp(_) => Err(NetError::refused(
                operation,
                self.describe(),
                format!(
                    "{option} is a TCP option and this is {}. A device answers ENOPROTOOPT; this \
                     seam refuses by name rather than accepting an option that could not take \
                     effect",
                    self.kind.as_str()
                ),
            )),
        }
    }

    /// Refuse an IPv6-only option on an IPv4 socket.
    fn require_v6(&self, operation: &'static str) -> NetResult<()> {
        if self.family == IpFamily::V6 {
            return Ok(());
        }
        Err(NetError::refused(
            operation,
            self.describe(),
            "IPV6_V6ONLY is an IPv6 option and this socket is IPv4, where the option does not \
             exist. It is refused rather than accepted, because a caller that set it and was told \
             it worked would believe this socket's address space had changed",
        ))
    }

    /// Refuse an address whose family is not this socket's.
    ///
    /// A device answers `EAFNOSUPPORT` and this does too. The alternative — converting an IPv4
    /// address into its IPv6-mapped form so that it "works" on a v6 socket — is a conversion the
    /// caller did not ask for, and it succeeds or fails depending on `IPV6_V6ONLY`, which is a
    /// second piece of state nothing at the call site can see.
    fn require_family(&self, operation: &'static str, address: &SocketAddress) -> NetResult<()> {
        if address.family() == self.family {
            return Ok(());
        }
        Err(NetError::kinded(
            operation,
            address.to_string(),
            NetErrorKind::AddressFamilyNotSupported,
            format!(
                "the address is {} and the socket was created for {}. This seam does not convert \
                 between families: whether an IPv4-mapped address reaches a v4 host depends on \
                 IPV6_V6ONLY, which the call site cannot see",
                address.family(),
                self.family
            ),
        ))
    }

    /// How this socket is named in a message.
    fn describe(&self) -> String {
        match self.local_address_quietly() {
            Some(local) => format!("{} {} bound to {local}", self.family, self.kind.as_str()),
            None => format!("{} {}", self.family, self.kind.as_str()),
        }
    }

    /// The local address, or nothing — for a message, where a failure to name the socket must not
    /// replace the failure being reported.
    fn local_address_quietly(&self) -> Option<std::net::SocketAddr> {
        match &self.inner {
            Inner::Tcp(stream) => stream.local_addr().ok(),
            Inner::Udp(socket) => socket.local_addr().ok(),
        }
    }
}

/// A zero timeout is "no timeout" on a device, and `std` spells that `None`.
///
/// `setsockopt(SO_RCVTIMEO)` with a zero `timeval` clears the timeout; `std` rejects
/// `Some(Duration::ZERO)` with `EINVAL` instead. Translating here rather than passing the error on
/// means a guest that clears its timeout the way C does is not told its call was invalid.
/// How the three keep-alive timing options name themselves in a refusal.
///
/// **Both spellings, deliberately.** A refusal is read by somebody holding either a Linux number
/// or a Windows one, and this seam's whole job here is that the two are not the same option. A
/// message naming one of them would send half its readers to the wrong constant. See
/// [`SocketOption::KeepAliveIdle`] for the table.
const KEEP_ALIVE_IDLE: &str = "the keep-alive idle time (Linux TCP_KEEPIDLE / Windows TCP_KEEPALIVE)";
/// See [`KEEP_ALIVE_IDLE`].
const KEEP_ALIVE_INTERVAL: &str = "the keep-alive probe interval (TCP_KEEPINTVL)";
/// See [`KEEP_ALIVE_IDLE`].
const KEEP_ALIVE_COUNT: &str = "the keep-alive probe count (TCP_KEEPCNT)";

/// A [`Duration`] as the whole seconds both hosts count these options in, or a refusal.
///
/// **Two refusals rather than two roundings**, and each is the shape rule 1 is about:
///
/// * A duration with a fractional part cannot be expressed. `setsockopt` takes an integer number
///   of seconds on Linux and a `DWORD` of seconds on Windows, so 500 ms is either 0 — probe
///   immediately, which is not what anybody means — or 1, which is twice what was asked. Refusing
///   says so; rounding sets an interval the caller never chose and reports success.
/// * A duration past `u32::MAX` seconds (136 years) does not fit the host's word and would wrap.
///
/// Neither is reachable from the guest adapter, which carries an `int` of seconds and validates
/// its sign before it gets here. They are reachable from this crate's own API, which is what
/// makes them checks rather than VERIFICATION entry 12's unreachable guard, and the test
/// `a_fractional_keep_alive_interval_is_refused_rather_than_rounded` constructs both.
fn whole_seconds(operation: &'static str, option: &'static str, value: Duration) -> NetResult<u32> {
    if value.subsec_nanos() != 0 {
        return Err(NetError::kinded(
            operation,
            option,
            NetErrorKind::InvalidInput,
            format!(
                "{option} is counted in whole seconds on every supported host, and {value:?} is \
                 not one. It is refused rather than rounded, because a rounded interval is a \
                 value the caller never asked for reported as though it had been set"
            ),
        ));
    }
    u32::try_from(value.as_secs()).map_err(|_| {
        NetError::kinded(
            operation,
            option,
            NetErrorKind::InvalidInput,
            format!(
                "{option} was given {} seconds, which does not fit the 32-bit field both hosts \
                 carry it in",
                value.as_secs()
            ),
        )
    })
}

fn normalise_timeout(timeout: Option<Duration>) -> Option<Duration> {
    match timeout {
        Some(duration) if duration.is_zero() => None,
        other => other,
    }
}

/// Wait until one of these sockets is ready, or until `timeout` elapses.
///
/// Returns how many entries came back with something set. Each entry's answer is read with
/// [`PollEntry::readiness`].
///
/// # There is no infinite wait, on purpose
///
/// The timeout is a [`Duration`] and there is no overload without one, for the reason
/// [`pipe`](crate::fs::pipe) gives: D16's runaway-guest defence is built from step budgets a
/// sleeping thread does not consume, so a host thread parked for ever is unrecoverable. The
/// adapter above already caps `poll`, `select` and `nanosleep` and refuses an unbounded wait by
/// name; this seam gives it nothing to cap.
///
/// # Waiting on sockets *and* pipes at once
///
/// A guest `poll` names a mixed set — pipes and eventfds, whose readiness is in-process, and
/// sockets, whose readiness is the host's. There is no single call that waits on both, because the
/// in-process half is a condition variable ([`ReadyGate`](crate::fs::pipe::ReadyGate)) and the
/// socket half is a kernel object. The caller therefore alternates: test everything, then wait a
/// **slice** on whichever side can wait, then test again. Reading
/// [`ReadyGate::generation`](crate::fs::pipe::ReadyGate::generation) *before* the test rather than
/// after the failure is what makes that loop correct — the other order loses a write that lands in
/// between, which is `VERIFICATION.md` entry 11's 1.0104-second lost wakeup.
///
/// # Errors
///
/// * [`NetError::Refused`] for more than [`MAX_POLL_SOCKETS`] entries. Refused rather than
///   truncated: a truncated poll reports "nothing is ready" about sockets it never looked at.
/// * [`NetError::Unsupported`] on Linux and macOS, naming `poll(2)`.
/// * [`NetError::Io`] when the host's readiness call fails.
pub fn poll(entries: &mut [PollEntry<'_>], timeout: Duration) -> NetResult<usize> {
    if entries.is_empty() {
        // **This does not sleep**, and the difference matters: a caller with no sockets to watch
        // is a caller waiting on something else, and burning its timeout here would delay the
        // in-process half of a mixed wait by exactly the slice it asked this side to cover.
        return Ok(0);
    }
    if entries.len() > MAX_POLL_SOCKETS {
        return Err(NetError::refused(
            "poll",
            format!("{} sockets", entries.len()),
            format!(
                "this seam polls at most {MAX_POLL_SOCKETS} sockets in one call, because the \
                 Windows backend uses `select` and an FD_SET holds FD_SETSIZE = \
                 {MAX_POLL_SOCKETS} entries. The set is refused rather than truncated: a \
                 truncated poll answers `not ready` about sockets it never looked at, which a \
                 caller cannot tell from a timeout. Lifting it means either chunking the set or \
                 moving to WSAPoll, and WSAPoll is not a drop-in — see this module's `poll`"
            ),
        ));
    }
    backend::poll(entries, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh entry has been told nothing, and says so rather than defaulting to ready.
    ///
    /// `Readiness::ALWAYS` is the right answer for a regular file and the worst available one for
    /// a socket that has not been polled: a caller acting on it would send into a buffer it has no
    /// evidence has room.
    #[test]
    fn an_unpolled_entry_reports_nothing_rather_than_ready() {
        let fresh = NOT_READY;
        assert_eq!(
            fresh,
            Readiness { readable: false, writable: false, hangup: false, error: false },
            "an entry that has not been polled must claim nothing"
        );
        assert_ne!(
            fresh,
            Readiness::ALWAYS,
            "ALWAYS is a regular file's answer and is the worst available one for a socket"
        );
    }

    /// An empty poll returns immediately and does not consume its timeout.
    ///
    /// Asserted on the clock, because the defect it guards against — a caller's mixed wait being
    /// delayed by the slice it asked the socket side to cover — is invisible in the return value.
    #[test]
    fn polling_no_sockets_returns_at_once_instead_of_sleeping() {
        let started = std::time::Instant::now();
        let count = poll(&mut [], Duration::from_secs(5)).expect("an empty poll cannot fail");
        assert_eq!(count, 0);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "an empty poll slept: {:?}",
            started.elapsed()
        );
    }

    /// A zero timeout is "no timeout", which is what a zero `timeval` means to `setsockopt`.
    #[test]
    fn a_zero_timeout_means_no_timeout_rather_than_an_invalid_argument() {
        assert_eq!(normalise_timeout(Some(Duration::ZERO)), None);
        assert_eq!(normalise_timeout(None), None);
        assert_eq!(
            normalise_timeout(Some(Duration::from_millis(250))),
            Some(Duration::from_millis(250))
        );
    }

    /// A keep-alive figure that is not a whole number of seconds is refused, not rounded.
    ///
    /// The unit-level half of `net_loopback.rs`'s socket-level test, and it is here because this
    /// is the only place the two failure paths can be reached without a host socket -- which is
    /// what makes them checks rather than VERIFICATION entry 12's unreachable guard.
    #[test]
    fn a_keep_alive_figure_must_be_whole_seconds_and_fit_the_hosts_field() {
        assert_eq!(
            whole_seconds("setsockopt", KEEP_ALIVE_IDLE, Duration::from_secs(7200)).ok(),
            Some(7200)
        );
        assert_eq!(whole_seconds("setsockopt", KEEP_ALIVE_IDLE, Duration::ZERO).ok(), Some(0));
        let fraction = whole_seconds("setsockopt", KEEP_ALIVE_INTERVAL, Duration::new(30, 1))
            .expect_err("30.000000001s is not a whole number of seconds");
        assert_eq!(fraction.kind(), Some(NetErrorKind::InvalidInput));
        assert!(fraction.to_string().contains("whole seconds"), "{fraction}");
        let huge = whole_seconds(
            "setsockopt",
            KEEP_ALIVE_COUNT,
            Duration::from_secs(u64::from(u32::MAX) + 1),
        )
        .expect_err("past the 32-bit field");
        assert_eq!(huge.kind(), Some(NetErrorKind::InvalidInput));
    }

    /// The interest constants are what their names say, with no overlap or omission.
    #[test]
    fn the_interest_constants_cover_both_directions_and_no_more() {
        assert_eq!(Interest::READABLE, Interest { readable: true, writable: false });
        assert_eq!(Interest::WRITABLE, Interest { readable: false, writable: true });
        assert_eq!(Interest::BOTH, Interest { readable: true, writable: true });
    }

    /// Every socket kind renders distinctly, so a refusal can say which one it was.
    #[test]
    fn the_socket_kinds_are_distinct_and_name_their_posix_spelling() {
        let names: Vec<&str> = SocketKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
        assert!(names[0].contains("SOCK_STREAM"), "{}", names[0]);
        assert!(names[1].contains("SOCK_DGRAM"), "{}", names[1]);
    }

    /// The list of implemented options names every variant of `SocketOption`.
    ///
    /// **Membership, not a count** (VERIFICATION entry 1): the string is what a caller reads when
    /// its option is refused, and a string that had drifted from the enum would send somebody
    /// looking for an option that is there, or stop them adding one that is not.
    #[test]
    fn the_implemented_options_string_names_every_option_that_is_implemented() {
        for spelling in [
            "SO_ERROR",
            "SO_REUSEADDR",
            "SO_KEEPALIVE",
            "TCP_NODELAY",
            "SO_RCVBUF",
            "SO_SNDBUF",
            "SO_RCVTIMEO",
            "SO_SNDTIMEO",
            "IPV6_V6ONLY",
            // The three keep-alive TIMING options. Named in the LINUX spelling, because the
            // guest whose refusal reads this string is a Linux binary -- with Windows' name for
            // the first one beside it, since that is the one pair whose names differ.
            "TCP_KEEPIDLE",
            "TCP_KEEPALIVE",
            "TCP_KEEPINTVL",
            "TCP_KEEPCNT",
        ] {
            assert!(
                IMPLEMENTED_OPTIONS.contains(spelling),
                "{spelling} is implemented and is not in IMPLEMENTED_OPTIONS"
            );
        }
        // And nothing that is *not* implemented is advertised as though it were.
        // `SO_KEEPALIVE` was on this list until Roblox's HTTP stack set it on the settings socket
        // and the refusal killed the fetch thread. It moved to the list above rather than being
        // deleted from this one, which is the whole point of having both: a symbol that becomes
        // implemented has to be *moved*, and a `contains` test that only checked the positive half
        // would have let the advertisement and the implementation drift apart in silence.
        // `TCP_MAXRT` is on this list for a reason worth stating: it is Windows' option 5, and
        // Linux's `TCP_KEEPINTVL` is 5 too. A pass-through implementation would set it while
        // believing it had set the probe interval. Nothing here implements it, and nothing
        // should advertise it.
        for absent in ["SO_LINGER", "SO_BROADCAST", "IP_TTL", "SO_OOBINLINE", "TCP_MAXRT"] {
            assert!(
                !IMPLEMENTED_OPTIONS.contains(absent),
                "{absent} is advertised as implemented and is not"
            );
        }
    }
}
