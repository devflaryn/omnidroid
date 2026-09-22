//! Name resolution: host + port to a list of addresses, with the failure cases kept apart.
//!
//! # This is `std` and has no backend, and that is the D23 test answering "yes"
//!
//! [`std::net::ToSocketAddrs`] is `getaddrinfo(3)` on all three unix targets and
//! `GetAddrInfoW`/`getaddrinfo` on Windows, reached through **one** portable call. D23's sharper
//! test — *is there one `std` call that serves all five targets?* — answers yes here, so this
//! module is written once, has no `cfg`, and gets **no fabricated `Unsupported` arm** for Linux or
//! macOS, per D22's other half. It is the half of D30's "partly" that came out on the `std` side.
//!
//! # Why the classification cannot come from `ErrorKind`, and where it comes from instead
//!
//! `getaddrinfo` does not fail with an errno. It fails with `EAI_*`, a separate numbering with a
//! separate `gai_strerror`, and the distinction a caller acts on is *which failures are worth
//! retrying* — see [`ResolveFailure::is_retryable`]. `std::io::ErrorKind` has no variants for any
//! of it: every resolver failure arrives as an unclassified `io::Error`, and the only thing in it
//! that can be told apart is [`std::io::Error::raw_os_error`].
//!
//! So the classification here is a table of **host resolver error numbers**, and the honest
//! statement about it is precise:
//!
//! * **Windows has been measured.** `getaddrinfo` there returns WSA error codes directly and
//!   `std` wraps them with `io::Error::from_raw_os_error`, so `raw_os_error()` is `Some` and the
//!   numbers in [`WSA_HOST_NOT_FOUND`] and its neighbours are what arrives.
//! * **The unix targets have not.** `std`'s unix path turns a non-`EAI_SYSTEM` failure into an
//!   `io::Error` built from `gai_strerror`'s *text* with no raw code at all, so `raw_os_error()`
//!   is `None` there and nothing in this table matches. Such a failure is reported as
//!   [`ResolveFailure::Unclassified`], which the adapter above must refuse by name.
//!
//! That is a real gap on four of the five targets and it is written down rather than papered
//! over. What would close it is a resolver call that reports `EAI_*` directly, which means
//! `libc::getaddrinfo` in a unix backend — the same shape the readiness backend has, and worth
//! doing on the day somebody can run it.
//!
//! # `NoAddressOfFamily` is derived rather than reported
//!
//! A guest asking for `AF_INET6` addresses of a name that has only `A` records must get
//! `EAI_ADDRFAMILY` and not `EAI_NONAME`, because the two mean different things to a client that
//! is trying both families. No host reports it for us here — `to_socket_addrs` takes no family —
//! so this module asks for **everything** and filters, and "the unfiltered answer had addresses
//! and the filtered one did not" is a fact it can state on its own. That is the one classification
//! here that does not depend on a host error number, and it therefore works identically on all
//! five targets.
//!
//! # What this deliberately does not do
//!
//! * **No service names.** `getaddrinfo(host, "https")` needs `getservbyname(3)` and a services
//!   database; this seam takes a port number, and [`service_port`] refuses a name **by name**
//!   rather than guessing that `https` is 443. A guessed table is a claim about the host's
//!   `/etc/services` that this crate cannot check, and the one place a wrong entry surfaces is a
//!   connection to the wrong port.
//! * **No `AI_CANONNAME`.** `to_socket_addrs` does not return one, so nothing here can. A guest
//!   that asks for it gets `ai_canonname` left null by the adapter, which is what `getaddrinfo`
//!   returns when the flag is not set.
//! * **No reverse lookup.** `getnameinfo` is not in the import list and nothing has reached it.

use std::net::ToSocketAddrs;

use super::address::{IpFamily, SocketAddress};
use super::error::{NetError, NetResult, ResolveFailure};
use super::policy::NetPolicy;

/// `WSAHOST_NOT_FOUND`: the name does not exist. `EAI_NONAME`'s Windows number.
///
/// Spelled as a literal rather than imported from `windows-sys`, for the reason
/// `fs::windows`'s `FILE_READ_ONLY_VOLUME` is: this module has no `cfg` and must not grow one,
/// and the value is stable published API. It is matched against
/// [`std::io::Error::raw_os_error`], which is `None` on a host that does not use this numbering,
/// so a literal here cannot produce a wrong answer on another target — only an unclassified one.
pub const WSA_HOST_NOT_FOUND: i32 = 11_001;
/// `WSATRY_AGAIN`: the resolver did not answer. `EAI_AGAIN`'s Windows number.
pub const WSA_TRY_AGAIN: i32 = 11_002;
/// `WSANO_RECOVERY`: a permanent resolver failure. `EAI_FAIL`'s Windows number.
pub const WSA_NO_RECOVERY: i32 = 11_003;
/// `WSANO_DATA`: the name exists with no record of the type asked for. `EAI_NODATA`'s number.
pub const WSA_NO_DATA: i32 = 11_004;

/// Look a name up and return every address it has, filtered to one family if asked.
///
/// `want` of `None` asks for both families, which is `AF_UNSPEC` — the thing a client that will
/// try IPv6 and fall back to IPv4 wants, and what the engine's own HTTP stack asks for.
///
/// The returned list is in the resolver's own order. **That order is not sorted here**, and the
/// reason is worth stating: `getaddrinfo` is required by RFC 6724 to return addresses in a
/// destination-preference order the *host's* stack computes from its own routing table, and a
/// re-sort by this layer would discard a decision made with information this layer does not have.
/// A caller that wants a particular family asks for it rather than reordering.
///
/// # Errors
///
/// * [`NetError::Policy`] when the embedding's policy does not admit the name — **before** any
///   query leaves this machine.
/// * [`NetError::Refused`] for a host this seam will not look up at all, e.g. an empty name.
/// * [`NetError::Resolve`] carrying a [`ResolveFailure`] for everything else. The failure is the
///   thing the caller acts on; see [`ResolveFailure::is_retryable`].
pub fn resolve(
    host: &str,
    port: u16,
    want: Option<IpFamily>,
    policy: &NetPolicy,
) -> NetResult<Vec<SocketAddress>> {
    const OP: &str = "getaddrinfo";
    policy.check_host(OP, host, port)?;

    let trimmed = host.trim();
    if trimmed.is_empty() {
        // A null or empty `node` argument asks `getaddrinfo` for the loopback address, or for the
        // wildcard when `AI_PASSIVE` is set. Both are *listening* questions, and this seam has no
        // `listen` and no `accept`; answering one would be inventing a use for a result nothing
        // here can consume.
        return Err(NetError::refused(
            OP,
            format!("(empty host):{port}"),
            "an empty node name asks getaddrinfo for the local host, which is a question about \
             listening. This seam implements outgoing connections only — there is no `listen` and \
             no `accept` here — so the call is refused rather than answered with an address \
             nothing could use. Pass a name or an address literal",
        ));
    }

    let all: Vec<SocketAddress> = match (trimmed, port).to_socket_addrs() {
        Ok(addresses) => addresses.map(SocketAddress::from_std).collect(),
        Err(error) => return Err(classify(trimmed, port, &error)),
    };

    if all.is_empty() {
        // A successful `getaddrinfo` with an empty list is not something any host is supposed to
        // produce. It is reported rather than silently returned as an empty vector, because a
        // caller that received `Ok(vec![])` would have to invent its own failure for it.
        return Err(NetError::Resolve {
            host: trimmed.to_owned(),
            port,
            failure: ResolveFailure::NoSuchHost,
            detail: "the host resolver succeeded and returned no addresses at all".to_owned(),
        });
    }

    let Some(family) = want else {
        return Ok(all);
    };

    let wanted: Vec<SocketAddress> =
        all.iter().copied().filter(|address| address.family() == family).collect();
    if wanted.is_empty() {
        let families: Vec<&str> = {
            let mut seen: Vec<&str> = all.iter().map(|a| a.family().as_str()).collect();
            seen.sort_unstable();
            seen.dedup();
            seen
        };
        return Err(NetError::Resolve {
            host: trimmed.to_owned(),
            port,
            failure: ResolveFailure::NoAddressOfFamily,
            detail: format!(
                "the name resolved to {} address(es), all of them {}, and {} was asked for. \
                 Derived here rather than reported by the host: this seam asks for every family \
                 and filters, because `to_socket_addrs` takes none",
                all.len(),
                families.join(" and "),
                family
            ),
        });
    }
    Ok(wanted)
}

/// Turn a host resolver failure into the class a caller acts on.
///
/// See this module's header for why the table is host error *numbers* and why only Windows'
/// numbering has been measured.
fn classify(host: &str, port: u16, error: &std::io::Error) -> NetError {
    let code = error.raw_os_error();
    let failure = match code {
        Some(WSA_HOST_NOT_FOUND) => ResolveFailure::NoSuchHost,
        Some(WSA_TRY_AGAIN) => ResolveFailure::Transient,
        Some(WSA_NO_RECOVERY) => ResolveFailure::NonRecoverable,
        Some(WSA_NO_DATA) => ResolveFailure::NoAddressOfFamily,
        _ => ResolveFailure::Unclassified,
    };
    let detail = match code {
        Some(code) => format!("{error} (host resolver error {code})"),
        None => format!(
            "{error} (the host reported no error number, so this seam has nothing to classify \
             it by — see omni-platform's net::resolve for why that is the unix case and what \
             would close it)"
        ),
    };
    NetError::Resolve { host: host.to_owned(), port, failure, detail }
}

/// A `getaddrinfo` service argument to a port number, or a refusal that names what is missing.
///
/// `getaddrinfo(host, "443")` works. `getaddrinfo(host, "https")` does **not**, and is refused by
/// name rather than answered from a table this crate invented: a services database belongs to the
/// host, `getservbyname(3)` is how it is read, and the one place a wrong guess surfaces is a
/// connection to the wrong port — which looks like a network failure and is not one.
///
/// # Errors
///
/// [`NetError::Refused`] for anything that is not a decimal port number, naming the POSIX call
/// that would implement it.
pub fn service_port(service: &str) -> NetResult<u16> {
    let trimmed = service.trim();
    trimmed.parse::<u16>().map_err(|_| {
        NetError::refused(
            "getaddrinfo",
            format!("service `{service}`"),
            "this seam takes a numeric service only. Resolving a service *name* needs \
             `getservbyname(3)` and the host's services database, and neither is implemented \
             here; a built-in name table would be a claim about the host's /etc/services that \
             this crate cannot check, and a wrong entry surfaces as a connection to the wrong \
             port rather than as an error",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An address literal resolves without any query leaving the machine.
    ///
    /// This is the one resolution that can be asserted on every target with no network: `std`
    /// parses a literal itself rather than asking a resolver, so the test is about this module's
    /// filtering and error shaping and needs nothing from the host but arithmetic.
    #[test]
    fn an_address_literal_resolves_to_itself_without_a_query() {
        let policy = NetPolicy::loopback_only();
        let addresses = resolve("127.0.0.1", 443, None, &policy).expect("a literal");
        assert_eq!(addresses, vec![SocketAddress::V4 { address: [127, 0, 0, 1], port: 443 }]);

        let v6 = resolve("::1", 443, None, &policy).expect("a v6 literal");
        assert_eq!(v6, vec![SocketAddress::loopback(IpFamily::V6, 443)]);
    }

    /// Asking for the family a literal does not have gets `NoAddressOfFamily`, not `NoSuchHost`.
    ///
    /// The distinction is the whole reason the filter is here: a client trying IPv6 first and
    /// falling back needs to know the name exists.
    #[test]
    fn a_family_the_name_does_not_have_is_reported_as_such_and_not_as_a_missing_name() {
        let policy = NetPolicy::loopback_only();
        let err = resolve("127.0.0.1", 443, Some(IpFamily::V6), &policy).unwrap_err();
        assert_eq!(err.resolve_failure(), Some(ResolveFailure::NoAddressOfFamily));
        assert!(!err.resolve_failure().is_some_and(ResolveFailure::is_retryable));
        let text = err.to_string();
        assert!(text.contains("IPv4"), "the message says what it did find: {text}");
        assert!(text.contains("IPv6"), "and what was asked for: {text}");
    }

    /// The policy is consulted before the resolver is, so a refused name never leaves the machine.
    #[test]
    fn a_name_outside_the_policy_is_refused_before_any_query() {
        let policy = NetPolicy::closed();
        let err = resolve("clientsettingscdn.roblox.com", 443, None, &policy).unwrap_err();
        assert!(matches!(err, NetError::Policy { .. }), "{err}");
        assert_eq!(err.resolve_failure(), None, "a policy refusal is not a resolver failure");
    }

    /// An empty name is refused by name rather than answered with a listening address.
    #[test]
    fn an_empty_host_is_refused_and_the_message_says_why_it_cannot_be_answered() {
        let err = resolve("   ", 443, None, &NetPolicy::unrestricted()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("listen"), "{text}");
        assert!(matches!(err, NetError::Refused { .. }), "{err}");
    }

    /// A numeric service parses and a service name is refused by name.
    #[test]
    fn a_service_name_is_refused_and_a_numeric_service_is_not() {
        assert_eq!(service_port("443").unwrap(), 443);
        assert_eq!(service_port(" 80 ").unwrap(), 80);
        assert_eq!(service_port("0").unwrap(), 0);
        for name in ["https", "http", "domain", ""] {
            let err = service_port(name).unwrap_err();
            assert!(err.to_string().contains("getservbyname(3)"), "{err}");
        }
        // A port past 16 bits is not a port, and is refused rather than truncated.
        assert!(service_port("65536").is_err());
    }

    /// The Windows resolver numbers map to the classes a caller acts on.
    ///
    /// Constructed from the raw codes rather than by provoking a real lookup, because a test that
    /// needed a DNS server would be a test that passes or fails on somebody's network — and the
    /// thing under test is the table, not the resolver.
    #[test]
    fn the_measured_resolver_numbers_classify_and_an_unknown_one_does_not() {
        let cases = [
            (WSA_HOST_NOT_FOUND, ResolveFailure::NoSuchHost, false),
            (WSA_TRY_AGAIN, ResolveFailure::Transient, true),
            (WSA_NO_RECOVERY, ResolveFailure::NonRecoverable, false),
            (WSA_NO_DATA, ResolveFailure::NoAddressOfFamily, false),
            // A number that is not in the table: reported unclassified, never guessed at.
            (1_234_567, ResolveFailure::Unclassified, false),
        ];
        for (code, expected, retryable) in cases {
            let error = std::io::Error::from_raw_os_error(code);
            let classified = classify("clientsettingscdn.roblox.com", 443, &error);
            assert_eq!(
                classified.resolve_failure(),
                Some(expected),
                "host resolver error {code} classified wrongly"
            );
            assert_eq!(expected.is_retryable(), retryable);
            assert!(classified.to_string().contains(&code.to_string()), "{classified}");
        }
    }

    /// A resolver failure with no host error number is unclassified and says why.
    ///
    /// This is the unix case, and the assertion exists so that the gap is visible in the suite on
    /// the host where it cannot be reproduced.
    #[test]
    fn a_resolver_failure_with_no_number_is_unclassified_and_explains_itself() {
        let error = std::io::Error::other("Name or service not known");
        let classified = classify("example.invalid", 443, &error);
        assert_eq!(classified.resolve_failure(), Some(ResolveFailure::Unclassified));
        let text = classified.to_string();
        assert!(text.contains("no error number"), "{text}");
    }
}
