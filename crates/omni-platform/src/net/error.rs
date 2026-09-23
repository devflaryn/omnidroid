//! Typed, diagnostic errors for the network seam.
//!
//! The same discipline as [`FsError`](crate::fs::FsError) and
//! [`WindowError`](crate::window::WindowError): every variant names the operation and the values
//! it failed with (Global Constraint 7), and there is deliberately no catch-all `Other(String)`.
//!
//! # Why the *kind* is a type of ours rather than an errno
//!
//! [`FsError`](crate::fs::FsError)'s reason, unchanged: the caller that turns a failure into the
//! guest's `errno` is `omni-android`'s adapter, and the numbering it must use is the **guest's** —
//! Linux arm64's, which `omni-bionic::errno` owns and which is not this host's. `omni-platform`
//! sits below both crates and cannot depend on either, so it reports a classified
//! [`NetErrorKind`] and the adapter maps it. One table of guest errno numbers in the workspace
//! instead of two that can drift.
//!
//! # Resolution fails in kinds a caller *acts* on differently, so it gets its own classification
//!
//! Every other failure here is an errno. A name lookup is not: `getaddrinfo` returns `EAI_*`,
//! which is a separate numbering with a separate `gai_strerror`, and the distinction that matters
//! to a caller is **which failures are worth retrying**. `EAI_AGAIN` says the resolver could not
//! reach a server and the same question asked again may succeed; `EAI_NONAME` says the name does
//! not exist and asking again will get the same answer for ever. A client that retries the second
//! one spins, and a client that gives up on the first one fails a session that a second attempt
//! would have completed.
//!
//! [`ResolveFailure`] therefore carries [`ResolveFailure::is_retryable`], and it is the *only*
//! place in this crate where an error kind is asked a question rather than merely rendered.
//!
//! # `Unclassified` is a refusal, not a guess
//!
//! [`ResolveFailure::Unclassified`] exists because the host's own resolver error reaches this
//! layer as a number in `std::io::Error::raw_os_error`, and that number has only been *measured*
//! on Windows. A failure whose number is not in the table is reported as unclassified with the
//! host's text kept, and the adapter above must refuse the guest call by name rather than choose
//! an `EAI_*` for it. D30 is explicit about the trap: a deliberate `EAI_NONAME` that gets you past
//! a gate is indistinguishable, a week later, from an implementation that works.

use core::fmt;

/// Result alias for every operation on the network seam.
pub type NetResult<T> = Result<T, NetError>;

/// What kind of failure the host reported, in terms a guest `errno` can be derived from.
///
/// Deliberately small, and every variant is one an operation on this seam can actually produce.
/// The comment beside each names the Linux arm64 errno the adapter is expected to map it to —
/// which is documentation of intent, not a table this crate owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NetErrorKind {
    /// The call would have had to wait — `EAGAIN`, which on Linux is `EWOULDBLOCK`.
    ///
    /// **The single most load-bearing variant here.** A client that treats a would-block as an
    /// error closes a connection that was working; one that treats an error as a would-block
    /// spins for ever. Every send and receive on this seam reports it distinctly for that reason,
    /// and [`NetError::is_would_block`] exists so no caller has to match a variant shape to ask.
    WouldBlock,
    /// A non-blocking `connect` has been started and has not finished — `EINPROGRESS`.
    ///
    /// Distinct from [`WouldBlock`](Self::WouldBlock) because the guest's own `connect` must
    /// answer `EINPROGRESS` and not `EAGAIN`: a caller waits for writability after the first and
    /// retries the call after the second, and the two are not interchangeable.
    InProgress,
    /// `connect` on a socket that already has a peer — `EISCONN`.
    AlreadyConnected,
    /// A send or receive on a socket with no peer — `ENOTCONN`.
    NotConnected,
    /// The peer's host answered with a reset in place of an accept — `ECONNREFUSED`.
    ConnectionRefused,
    /// The peer reset an established connection — `ECONNRESET`.
    ConnectionReset,
    /// The connection was aborted locally — `ECONNABORTED`.
    ConnectionAborted,
    /// `bind` to an address and port something else already holds — `EADDRINUSE`.
    AddressInUse,
    /// `bind` to an address this host does not have — `EADDRNOTAVAIL`.
    AddressNotAvailable,
    /// No route to the destination network — `ENETUNREACH`.
    NetworkUnreachable,
    /// A route exists and the host did not answer — `EHOSTUNREACH`.
    HostUnreachable,
    /// The operation took longer than the stack was willing to wait — `ETIMEDOUT`.
    TimedOut,
    /// A write to a stream the peer has closed — `EPIPE`.
    ///
    /// On a device this also raises `SIGPIPE`. There is no signal delivery in this runtime (D24),
    /// so the errno is the whole of what the guest receives.
    BrokenPipe,
    /// The host refused the operation — `EACCES`.
    PermissionDenied,
    /// The arguments do not form a valid call — `EINVAL`.
    InvalidInput,
    /// The address's family is not the socket's — `EAFNOSUPPORT`.
    AddressFamilyNotSupported,
    /// A datagram larger than the path will carry — `EMSGSIZE`.
    MessageSize,
    /// The call was interrupted — `EINTR`.
    Interrupted,
    /// The stack has no buffer space left — `ENOBUFS`.
    NoBufferSpace,
    /// The host reported a failure this classification does not name.
    ///
    /// **Not mapped to an errno by the caller.** The adapter refuses such a call by name, because
    /// the alternative is answering a specific errno for a failure nobody identified — which is
    /// the plausible-wrong-answer shape this project keeps being bitten by.
    Other,
}

impl NetErrorKind {
    /// A short name for the kind, for a message that has to say what happened.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            NetErrorKind::WouldBlock => "resource temporarily unavailable",
            NetErrorKind::InProgress => "operation now in progress",
            NetErrorKind::AlreadyConnected => "transport endpoint is already connected",
            NetErrorKind::NotConnected => "transport endpoint is not connected",
            NetErrorKind::ConnectionRefused => "connection refused",
            NetErrorKind::ConnectionReset => "connection reset by peer",
            NetErrorKind::ConnectionAborted => "software caused connection abort",
            NetErrorKind::AddressInUse => "address already in use",
            NetErrorKind::AddressNotAvailable => "cannot assign requested address",
            NetErrorKind::NetworkUnreachable => "network is unreachable",
            NetErrorKind::HostUnreachable => "no route to host",
            NetErrorKind::TimedOut => "connection timed out",
            NetErrorKind::BrokenPipe => "broken pipe",
            NetErrorKind::PermissionDenied => "permission denied",
            NetErrorKind::InvalidInput => "invalid argument",
            NetErrorKind::AddressFamilyNotSupported => {
                "address family not supported by protocol"
            }
            NetErrorKind::MessageSize => "message too long",
            NetErrorKind::Interrupted => "interrupted system call",
            NetErrorKind::NoBufferSpace => "no buffer space available",
            NetErrorKind::Other => "an unclassified host error",
        }
    }

    /// Classify a host [`std::io::Error`] that came back from a `std::net` call.
    ///
    /// **`std::io::ErrorKind` and nothing else**, for [`FsErrorKind::classify`]'s reason: it is
    /// portable standard library, one implementation, no `cfg`, and it is the classification the
    /// same host error would get anywhere else in this crate.
    ///
    /// Two of the interesting cases cannot arrive here and it is worth saying which. `EINPROGRESS`
    /// has no `ErrorKind` at all, and it is produced only by `connect`, which this seam reaches
    /// through the per-OS backend rather than through `std` — so the backend classifies it from
    /// the raw code and this function never sees it. `EISCONN` likewise.
    ///
    /// `ErrorKind` is `#[non_exhaustive]`, so the catch-all is required rather than lazy — and it
    /// is the honest arm: a kind this match does not name is one nobody has decided an errno for.
    ///
    /// [`FsErrorKind::classify`]: crate::fs::FsErrorKind::classify
    #[must_use]
    pub fn classify(error: &std::io::Error) -> NetErrorKind {
        use std::io::ErrorKind as K;
        match error.kind() {
            K::WouldBlock => NetErrorKind::WouldBlock,
            K::NotConnected => NetErrorKind::NotConnected,
            K::ConnectionRefused => NetErrorKind::ConnectionRefused,
            K::ConnectionReset => NetErrorKind::ConnectionReset,
            K::ConnectionAborted => NetErrorKind::ConnectionAborted,
            K::AddrInUse => NetErrorKind::AddressInUse,
            K::AddrNotAvailable => NetErrorKind::AddressNotAvailable,
            K::NetworkUnreachable => NetErrorKind::NetworkUnreachable,
            K::HostUnreachable => NetErrorKind::HostUnreachable,
            K::TimedOut => NetErrorKind::TimedOut,
            K::BrokenPipe => NetErrorKind::BrokenPipe,
            K::PermissionDenied => NetErrorKind::PermissionDenied,
            K::InvalidInput | K::InvalidData => NetErrorKind::InvalidInput,
            K::Interrupted => NetErrorKind::Interrupted,
            _ => NetErrorKind::Other,
        }
    }
}

impl fmt::Display for NetErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a name lookup failed, in the terms `getaddrinfo`'s `EAI_*` numbering distinguishes.
///
/// The variants are the ones a caller *behaves* differently about. `EAI_MEMORY`, `EAI_SYSTEM`,
/// `EAI_BADFLAGS` and the rest are deliberately absent: none of them is reachable from this
/// seam's own API, which takes a host, a port and a family rather than a flags word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ResolveFailure {
    /// The name does not exist — `EAI_NONAME`.
    ///
    /// Permanent. A caller that retries this gets the same answer for ever.
    NoSuchHost,
    /// The name exists and has no address of the family that was asked for — `EAI_ADDRFAMILY`,
    /// and `EAI_NODATA` where a host has records of some other type.
    ///
    /// **Derived here rather than reported by the host**, in the ordinary case: this seam asks
    /// the resolver for every family and filters, so "the unfiltered answer had addresses and the
    /// filtered one did not" is a fact it can state without the host having a code for it. That
    /// is the one classification in this enum that does not depend on a host error number.
    NoAddressOfFamily,
    /// The resolver could not be reached, or answered `SERVFAIL` — `EAI_AGAIN`.
    ///
    /// **The one that is worth retrying**, and the reason [`ResolveFailure::is_retryable`] exists.
    Transient,
    /// The resolver reported a permanent failure that is not "no such name" — `EAI_FAIL`.
    NonRecoverable,
    /// The host reported a resolver failure whose number is not in this seam's table.
    ///
    /// **The adapter must refuse the guest's `getaddrinfo` by name rather than choose an `EAI_*`
    /// for this.** See the module header: D30 permits a measured failure path as a diagnostic and
    /// forbids treating one as finished behaviour, and a default `EAI_NONAME` for anything
    /// unrecognised is exactly the thing it forbids.
    Unclassified,
}

impl ResolveFailure {
    /// A short name for the failure, for a message that has to say what happened.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ResolveFailure::NoSuchHost => "no such host is known",
            ResolveFailure::NoAddressOfFamily => {
                "the host has no address of the requested family"
            }
            ResolveFailure::Transient => "a temporary failure in name resolution",
            ResolveFailure::NonRecoverable => "a non-recoverable failure in name resolution",
            ResolveFailure::Unclassified => "an unclassified resolver failure",
        }
    }

    /// Whether asking the same question again could plausibly get a different answer.
    ///
    /// **True for exactly one variant.** `EAI_AGAIN` means the resolver did not answer; every
    /// other failure here is a statement about the name itself, and repeating a question about a
    /// name that does not exist is how a client turns a failed login into a spin.
    ///
    /// [`Unclassified`](Self::Unclassified) is **not** retryable, and that is the conservative
    /// direction on purpose: the adapter is required to refuse the call by name for it, so a
    /// caller should never see it at all, and answering "retry" for a failure nobody identified
    /// would be the second guess on top of the first.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, ResolveFailure::Transient)
    }
}

impl fmt::Display for ResolveFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything that can go wrong on the network seam.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NetError {
    /// This backend does not implement the operation.
    ///
    /// Returned by the Linux and macOS backends, which are structural only, and by nothing else.
    /// `intended` names the POSIX call the implementation is expected to make, so that the
    /// refusal says what the missing work *is* rather than only that it is missing.
    #[error(
        "network operation `{operation}` is not implemented on {platform}: the intended \
         implementation is `{intended}`, and omni-platform's {platform} net backend is \
         structural only and has never been run (see docs/ARCHITECTURE.md \"Portability rule\")"
    )]
    Unsupported {
        /// The seam operation that was called, e.g. `"socket"`.
        operation: &'static str,
        /// The POSIX call the implementation is meant to make, e.g. `"socket(2)"`.
        intended: &'static str,
        /// The target the backend was compiled for, e.g. `"linux"`.
        platform: &'static str,
    },

    /// The host refused the operation, classified into something an errno can be derived from.
    #[error("`{operation}` on {endpoint}: {kind} ({detail})")]
    Io {
        /// The seam operation that was called.
        operation: &'static str,
        /// The socket or address the operation named, rendered for a human.
        endpoint: String,
        /// What kind of failure it was.
        kind: NetErrorKind,
        /// The host's own message, kept because an unclassified failure has nothing else.
        detail: String,
    },

    /// A name could not be resolved.
    #[error("`getaddrinfo` for `{host}` port {port}: {failure} ({detail})")]
    Resolve {
        /// The name that was looked up.
        host: String,
        /// The port that was asked for alongside it.
        port: u16,
        /// Which `EAI_*` class this is.
        failure: ResolveFailure,
        /// The host resolver's own message, or this seam's reasoning where it derived the class.
        detail: String,
    },

    /// The embedding's network policy does not cover this destination.
    ///
    /// **This is D30's replacement for the old blanket refusal**, and it is deliberately not an
    /// `Io` failure with a plausible errno. Global Constraint 8 said "no network access at
    /// runtime" and the owner withdrew it; what replaced it is not an open socket but a question
    /// an embedding answers — the shape [`Filesystem`](crate::fs::Filesystem)'s root already has.
    /// A destination outside the policy is a *configuration* fact, and reporting one as
    /// `ENETUNREACH` would hide it in the ordinary noise of a client failing over.
    #[error(
        "`{operation}` refused the destination {endpoint}: {why}. Which network an instance may \
         reach is set by the embedding through `omni_platform::net::NetPolicy`, and the default \
         is `NetPolicy::closed()` — nothing at all (D30: Global Constraint 8 was withdrawn in \
         favour of a policy, not in favour of an open socket)"
    )]
    Policy {
        /// The seam operation that was called.
        operation: &'static str,
        /// The destination that was refused, rendered for a human.
        endpoint: String,
        /// Which rule refused it.
        why: String,
    },

    /// `setsockopt` or `getsockopt` named an option this seam does not implement.
    ///
    /// **The variant rule 1 exists for.** A `setsockopt` that is silently accepted is the exact
    /// defect "no plausible stubs" is about: the caller believes the option took effect, behaves
    /// as though it had, and the failure surfaces somewhere else entirely. Every option outside
    /// [`SocketOption`](super::SocketOption) and [`SocketQuery`](super::SocketQuery) therefore
    /// reaches the guest as this error, carrying the level and name it asked for so that the next
    /// person knows which one to add rather than that "an option" was missing.
    #[error(
        "`{operation}` was asked for option {name} at level {level}, which omni-platform's net \
         seam does not implement. It is refused rather than accepted, because an option that is \
         silently ignored leaves the caller believing it took effect. The options that are \
         implemented are: {known}"
    )]
    UnimplementedOption {
        /// `"setsockopt"` or `"getsockopt"`.
        operation: &'static str,
        /// The guest's `level` argument, in the guest's own numbering.
        level: i32,
        /// The guest's `optname` argument, in the guest's own numbering.
        name: i32,
        /// The options this seam does implement, so the message says what *is* available.
        known: &'static str,
    },

    /// The operation is well-formed and this layer will not carry it out.
    ///
    /// A socket kind the operation is not defined on, a set larger than the readiness backend can
    /// express, a service name with no numeric form. Never a plausible substitute.
    #[error("`{operation}` on {endpoint}: {why}")]
    Refused {
        /// The seam operation that was called.
        operation: &'static str,
        /// The socket or address the operation named.
        endpoint: String,
        /// Why it cannot be carried out.
        why: String,
    },
}

impl NetError {
    /// True when this failure means "this backend has no implementation".
    ///
    /// Mirrors [`FsError::is_unsupported`](crate::fs::FsError::is_unsupported) and
    /// [`WindowError::is_unsupported`](crate::window::WindowError::is_unsupported) so that a
    /// caller can tell "this target has never been built out" from "the OS said no" without
    /// matching every variant shape.
    #[must_use]
    pub fn is_unsupported(&self) -> bool {
        matches!(self, NetError::Unsupported { .. })
    }

    /// The classified host failure, when there is one.
    ///
    /// `None` for every variant the adapter must refuse by name rather than turn into an errno.
    #[must_use]
    pub fn kind(&self) -> Option<NetErrorKind> {
        match self {
            NetError::Io { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// Whether this is the would-block answer, without the caller matching a variant shape.
    ///
    /// Exists because the distinction between *would block* and *failed* is the one a
    /// non-blocking client gets wrong, and a caller that has to write
    /// `matches!(e, NetError::Io { kind: NetErrorKind::WouldBlock, .. })` at every send and
    /// receive site will eventually write it wrong at one of them.
    #[must_use]
    pub fn is_would_block(&self) -> bool {
        self.kind() == Some(NetErrorKind::WouldBlock)
    }

    /// The resolver classification, when this is a resolution failure.
    #[must_use]
    pub fn resolve_failure(&self) -> Option<ResolveFailure> {
        match self {
            NetError::Resolve { failure, .. } => Some(*failure),
            _ => None,
        }
    }

    /// Build the refusal for a socket option this seam does not implement.
    ///
    /// **Public, and that is the point.** The guest's `level` and `optname` are the *guest's*
    /// numbers, which `omni-android`'s adapter owns and this crate cannot see; so the adapter is
    /// the only place that can notice an option outside [`SocketOption`](super::SocketOption).
    /// Giving it the constructor means every such refusal is worded once, here, instead of the
    /// adapter inventing its own message — or, worse, returning `0` and letting the guest believe
    /// the option was set.
    #[must_use]
    pub fn unimplemented_option(operation: &'static str, level: i32, name: i32) -> NetError {
        NetError::UnimplementedOption {
            operation,
            level,
            name,
            known: super::IMPLEMENTED_OPTIONS,
        }
    }

    /// Build a [`NetError::Io`] from a host error.
    pub(super) fn io(
        operation: &'static str,
        endpoint: impl Into<String>,
        error: &std::io::Error,
    ) -> NetError {
        // `std` has no `ErrorKind` for every code a socket call produces -- `EMSGSIZE` among
        // them, on both families of host -- so the backend's own table of raw codes is asked
        // before a failure is left unclassified. MEASURED: a 1444-byte datagram with
        // don't-fragment set, over the path MTU, reached the guest as an unclassified refusal
        // rather than the `EMSGSIZE` `Socket::send_to` promises and ngtcp2's path-MTU discovery
        // handles.
        let kind = match NetErrorKind::classify(error) {
            NetErrorKind::Other => {
                error.raw_os_error().map_or(NetErrorKind::Other, super::backend::kind_from_raw)
            }
            kind => kind,
        };
        NetError::Io { operation, endpoint: endpoint.into(), kind, detail: error.to_string() }
    }

    /// Build a [`NetError::Io`] with a kind this seam decided rather than the host.
    pub(super) fn kinded(
        operation: &'static str,
        endpoint: impl Into<String>,
        kind: NetErrorKind,
        detail: impl Into<String>,
    ) -> NetError {
        NetError::Io { operation, endpoint: endpoint.into(), kind, detail: detail.into() }
    }

    /// Build a [`NetError::Refused`].
    pub(super) fn refused(
        operation: &'static str,
        endpoint: impl Into<String>,
        why: impl Into<String>,
    ) -> NetError {
        NetError::Refused { operation, endpoint: endpoint.into(), why: why.into() }
    }

    /// Build a [`NetError::Policy`].
    pub(super) fn policy(
        operation: &'static str,
        endpoint: impl Into<String>,
        why: impl Into<String>,
    ) -> NetError {
        NetError::Policy { operation, endpoint: endpoint.into(), why: why.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind has a distinct name, so a message can never be ambiguous about which one it is.
    ///
    /// Membership, not a total: VERIFICATION entry 1 is the record of a count that stayed right
    /// while two of its members were wrong, so this asserts the rendered set and compares its
    /// size against the list it was built from rather than against a literal.
    #[test]
    fn the_kinds_are_distinct_and_the_unclassified_one_carries_no_errno() {
        let all = [
            NetErrorKind::WouldBlock,
            NetErrorKind::InProgress,
            NetErrorKind::AlreadyConnected,
            NetErrorKind::NotConnected,
            NetErrorKind::ConnectionRefused,
            NetErrorKind::ConnectionReset,
            NetErrorKind::ConnectionAborted,
            NetErrorKind::AddressInUse,
            NetErrorKind::AddressNotAvailable,
            NetErrorKind::NetworkUnreachable,
            NetErrorKind::HostUnreachable,
            NetErrorKind::TimedOut,
            NetErrorKind::BrokenPipe,
            NetErrorKind::PermissionDenied,
            NetErrorKind::InvalidInput,
            NetErrorKind::AddressFamilyNotSupported,
            NetErrorKind::MessageSize,
            NetErrorKind::Interrupted,
            NetErrorKind::NoBufferSpace,
            NetErrorKind::Other,
        ];
        let mut names: Vec<&str> = all.iter().map(|kind| kind.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len(), "two kinds render the same way");
    }

    /// `WouldBlock` and `InProgress` are not the same answer and must never collapse.
    ///
    /// The guest's `connect` answers `EINPROGRESS` and its `send` answers `EAGAIN`; a caller waits
    /// for writability after the first and repeats the call after the second. A classification
    /// that mapped both to one kind would make the adapter choose, and it has no way to.
    #[test]
    fn a_pending_connect_and_a_full_send_buffer_are_different_kinds() {
        assert_ne!(NetErrorKind::WouldBlock, NetErrorKind::InProgress);
        assert_ne!(NetErrorKind::WouldBlock.as_str(), NetErrorKind::InProgress.as_str());
        let blocked = NetError::kinded("send", "fd 3", NetErrorKind::WouldBlock, "no room");
        let pending = NetError::kinded("connect", "fd 3", NetErrorKind::InProgress, "started");
        assert!(blocked.is_would_block());
        assert!(!pending.is_would_block(), "a pending connect is not a would-block");
    }

    /// The classification is the host's, and a failure the host does not classify stays `Other`.
    #[test]
    fn a_host_error_is_classified_and_an_unknown_one_is_not_guessed_at() {
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "nope");
        assert_eq!(NetErrorKind::classify(&refused), NetErrorKind::ConnectionRefused);
        let blocked = std::io::Error::new(std::io::ErrorKind::WouldBlock, "later");
        assert_eq!(NetErrorKind::classify(&blocked), NetErrorKind::WouldBlock);
        let weird = std::io::Error::other("a failure with no ErrorKind of its own");
        assert_eq!(
            NetErrorKind::classify(&weird),
            NetErrorKind::Other,
            "an unclassified failure must not be given a specific errno's kind"
        );
    }

    /// Exactly one resolver failure is worth retrying, and the unclassified one is not.
    #[test]
    fn only_a_transient_resolver_failure_is_retryable() {
        let all = [
            ResolveFailure::NoSuchHost,
            ResolveFailure::NoAddressOfFamily,
            ResolveFailure::Transient,
            ResolveFailure::NonRecoverable,
            ResolveFailure::Unclassified,
        ];
        let retryable: Vec<ResolveFailure> =
            all.iter().copied().filter(|f| f.is_retryable()).collect();
        assert_eq!(
            retryable,
            vec![ResolveFailure::Transient],
            "retrying a name that does not exist is how a client turns a failure into a spin"
        );
        let mut names: Vec<&str> = all.iter().map(|f| f.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len(), "two resolver failures render the same way");
    }

    /// Only `Io` carries a kind; every refusal deliberately does not.
    #[test]
    fn only_a_classified_host_failure_offers_a_kind() {
        let io = NetError::kinded("recv", "fd 3", NetErrorKind::NotConnected, "no peer");
        assert_eq!(io.kind(), Some(NetErrorKind::NotConnected));
        assert_eq!(NetError::refused("listen", "fd 3", "why").kind(), None);
        assert_eq!(NetError::policy("connect", "1.2.3.4:443", "why").kind(), None);
        let unsupported = NetError::Unsupported {
            operation: "socket",
            intended: "socket(2)",
            platform: "linux",
        };
        assert_eq!(unsupported.kind(), None);
        assert!(unsupported.is_unsupported());
        assert!(unsupported.to_string().contains("socket(2)"), "{unsupported}");
    }

    /// The option refusal names the numbers the caller passed and what *is* implemented.
    ///
    /// Global Constraint 7: a message that said only "unsupported option" would leave the next
    /// person grepping for which one, which is the state this variant exists to prevent.
    #[test]
    fn an_unimplemented_option_is_refused_by_number_and_says_what_is_available() {
        // `SOL_SOCKET` is 1 and `SO_LINGER` is 13 in the *guest's* numbering (Linux arm64), which
        // is the numbering the adapter passes and which this crate deliberately does not own.
        let err = NetError::unimplemented_option("setsockopt", 1, 13);
        let text = err.to_string();
        assert!(text.contains("option 13"), "{text}");
        assert!(text.contains("level 1"), "{text}");
        assert!(text.contains("SO_RCVBUF"), "the message must list what is implemented: {text}");
        assert_eq!(err.kind(), None, "an unimplemented option is not an errno this crate picks");
    }

    /// A resolution failure reports its class and keeps the host's own text.
    #[test]
    fn a_resolution_failure_carries_its_class_and_the_hosts_message() {
        let err = NetError::Resolve {
            host: "clientsettingscdn.roblox.com".to_owned(),
            port: 443,
            failure: ResolveFailure::Transient,
            detail: "WSATRY_AGAIN (11002)".to_owned(),
        };
        assert_eq!(err.resolve_failure(), Some(ResolveFailure::Transient));
        assert!(err.resolve_failure().is_some_and(ResolveFailure::is_retryable));
        let text = err.to_string();
        assert!(text.contains("clientsettingscdn.roblox.com"), "{text}");
        assert!(text.contains("11002"), "{text}");
    }
}
