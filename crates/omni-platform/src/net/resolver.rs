//! Name resolution: host + port to a list of addresses, with the failure cases kept apart.
//!
//! # The lookup is a backend; everything around it is written once
//!
//! This module used to be `std` with no backend, and that was D23's sharper test answering "yes":
//! [`std::net::ToSocketAddrs`] is `getaddrinfo(3)` on the unix targets and `getaddrinfo` on
//! Windows, reached through **one** portable call, so the module had no `cfg` and no fabricated
//! `Unsupported` arm (D22's other half). **The test was asked about the wrong half.** One `std`
//! call serves the *lookup* on all five targets; it does not serve the *failure*, and the failure
//! is what a caller acts on — see the next section. MEASURED what that cost (macOS run m10, +490
//! s): a lookup of `clientsettingscdn.roblox.com` failed during a DNS blip, `std` reported it as
//! text with no number, this module could only call it unclassified, the adapter refused the
//! guest's `getaddrinfo` by name as it must — and two guest threads died over a failure the engine
//! itself had a retry path for.
//!
//! So resolution is now split where D30 splits sockets — `std` where `std` serves, a per-OS
//! backend for the part it does not — and the split follows the readiness backend's rules: the
//! `cfg` is in `net/mod.rs`'s backend selection and nowhere else, and this file still has none.
//!
//! | piece | where | Linux / macOS |
//! |---|---|---|
//! | the policy, an empty name, a NUL, an address literal, the empty list, the family filter | **this file**, once for all five targets | the same code |
//! | the lookup and the classification of its failure | **backend** `lookup`: `std` plus the WSA table on Windows, **measured** | `libc::getaddrinfo` and the `EAI_*` table in `net/unix.rs`, with each target's `EAI_ADDRFAMILY` in `net/linux.rs` and `net/macos.rs` |
//!
//! # Why the classification cannot come from `ErrorKind`, and where it comes from instead
//!
//! `getaddrinfo` does not fail with an errno. It fails with `EAI_*`, a separate numbering with a
//! separate `gai_strerror`, and the distinction a caller acts on is *which failures are worth
//! retrying* — see [`ResolveFailure::is_retryable`]. `std::io::ErrorKind` has no variants for any
//! of it: every resolver failure arrives as an unclassified `io::Error`, and the only thing in it
//! that can be told apart is [`std::io::Error::raw_os_error`]. So the classification is a table of
//! **host resolver error numbers**, one per backend, and the honest statement about each is:
//!
//! * **Windows has been measured, and keeps `std`.** `getaddrinfo` there returns WSA error codes
//!   directly and `std` wraps them with `io::Error::from_raw_os_error`, so `raw_os_error()` is
//!   `Some` and `WSAHOST_NOT_FOUND` and its three neighbours are what arrives. Nothing about that
//!   path changed when the unix one did; the table moved into `net/windows.rs` with it.
//! * **The unix targets could not be classified through `std`, and no longer go through it.**
//!   `std`'s unix path turns a non-`EAI_SYSTEM` failure into an `io::Error` built from
//!   `gai_strerror`'s *text* with no raw code at all, so there was nothing to match. The unix
//!   backend calls `libc::getaddrinfo` itself, asking exactly the question `std` asked, and
//!   classifies the `EAI_*` it returns by each target's own `libc` constants — glibc counts down
//!   from -1 and Darwin up from 1, so a shared *table of names* is right on both and a shared
//!   table of numbers would be right on neither.
//!
//! A failure that neither table names is still [`ResolveFailure::Unclassified`], carrying the
//! code and the host's text, and the adapter above still refuses the guest's call by name for it.
//! What changed is that it is now a statement about *that code* rather than about a whole target.
//!
//! # `NoAddressOfFamily` is derived here as well as reported
//!
//! A guest asking for `AF_INET6` addresses of a name that has only `A` records must get
//! `EAI_ADDRFAMILY` and not `EAI_NONAME`, because the two mean different things to a client that
//! is trying both families. Every backend asks for **everything** (`AF_UNSPEC`) and this file
//! filters, so "the unfiltered answer had addresses and the filtered one did not" is a fact it can
//! state on its own, identically on all five targets. A host *can* also report the class — the
//! unix backend maps `EAI_ADDRFAMILY` and `EAI_NODATA` to it, Windows `WSANO_DATA` — but for an
//! `AF_UNSPEC` question that means "the name exists and has no address of *either* family", and
//! the class is the same one.
//!
//! # What this deliberately does not do
//!
//! * **No service names.** `getaddrinfo(host, "https")` needs `getservbyname(3)` and a services
//!   database; this seam takes a port number, and [`service_port`] refuses a name **by name**
//!   rather than guessing that `https` is 443. A guessed table is a claim about the host's
//!   `/etc/services` that this crate cannot check, and the one place a wrong entry surfaces is a
//!   connection to the wrong port. The unix backend passes the port as a decimal string under
//!   `AI_NUMERICSERV` for the same reason: no services database is consulted even by accident.
//! * **No `AI_CANONNAME`.** No backend asks for one — `to_socket_addrs` cannot, and the unix
//!   backend asks `std`'s question — so nothing here can return one. A guest that asks for it gets
//!   `ai_canonname` left null by the adapter, which is what `getaddrinfo` returns when the flag is
//!   not set.
//! * **No reverse lookup.** `getnameinfo` is not in the import list and nothing has reached it.

use std::net::{IpAddr, SocketAddr};

use super::address::{IpFamily, SocketAddress};
use super::backend;
use super::error::{NetError, NetResult, ResolveFailure};
use super::policy::NetPolicy;

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
/// * [`NetError::Refused`] for a host this seam will not look up at all: an empty name, or one
///   with a NUL in it.
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
    if trimmed.contains('\0') {
        // A C string ends at its first NUL, so the name a resolver would be asked about is not the
        // name the caller passed. `std` refuses this as `InvalidInput`; refusing it here, before
        // any backend, keeps the answer the same on every target. A guest cannot produce one (its
        // `node` is itself a C string), so this is a caller of the seam, not a guest, being told.
        return Err(NetError::refused(
            OP,
            format!("{trimmed:?}:{port}"),
            "the name contains a NUL byte, and the resolver is asked through a C string that \
             would end there — so it would be asked about a different name from the one passed",
        ));
    }

    // An address literal is answered without a resolver, as `std`'s `ToSocketAddrs` answers it:
    // one address, the port attached, no query. Done here rather than left to each backend so
    // that "a literal never leaves the machine" is one line on all five targets.
    let all: Vec<SocketAddress> = match trimmed.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddress::from_std(SocketAddr::new(ip, port))],
        Err(_) => backend::lookup(trimmed, port)?,
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
                 and filters",
                all.len(),
                families.join(" and "),
                family
            ),
        });
    }
    Ok(wanted)
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
    /// This is the one resolution that can be asserted on every target with no network: this
    /// module parses a literal itself before any backend is asked, so the test is about the
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

    /// A name with a NUL is refused before any backend, the same way on every target, and is not
    /// a resolver failure: no resolver was asked.
    #[test]
    fn a_name_with_a_nul_is_refused_before_any_backend_is_asked() {
        let err = resolve("example.invalid\0.com", 443, None, &NetPolicy::unrestricted())
            .unwrap_err();
        assert!(matches!(err, NetError::Refused { .. }), "{err}");
        assert_eq!(err.resolve_failure(), None, "{err}");
        assert!(err.to_string().contains("NUL"), "{err}");
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
}
