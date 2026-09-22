//! Which network an instance may reach: the question that replaced Global Constraint 8.
//!
//! # What this is the successor to
//!
//! Global Constraint 8 was "no network access at runtime", and it was never arbitrary: D6 records
//! that the APK under test is **cheat-injected and carries a Luau executor**, so guest code is
//! hostile by assumption, and a socket handed to it is an unrestricted host socket. `socket`,
//! `getaddrinfo` and `freeaddrinfo` were refused *by name* for that reason.
//!
//! **D30 withdrew the constraint and did not withdraw the threat.** The project owner's
//! instruction is quoted in that record; the operative half here is that what replaces the
//! refusal is *a policy an embedding sets*, in the shape [`Bionic::set_filesystem_root`] already
//! has for files. `set_filesystem_root` never meant "the guest gets no files" — it meant a guest
//! path resolves inside one host directory and nowhere else. This is the same sentence about
//! destinations: **which network may this instance reach is a question an embedding answers,
//! rather than one this layer has no way to ask.**
//!
//! [`Bionic::set_filesystem_root`]: https://docs.rs/omni-android
//!
//! # The default is closed, and it is closed by the type rather than by a check
//!
//! [`NetPolicy::default`] is [`NetPolicy::closed`], and a [`Socket`](super::Socket) cannot be
//! constructed without a policy — the constructor takes one, exactly as
//! [`Filesystem::new`](crate::fs::Filesystem::new) takes a root. So an embedding that has not
//! thought about the network gets a runtime whose every `connect` and `sendto` is refused by
//! name, naming this type; it does not get one that quietly reaches the internet because nobody
//! wrote a check.
//!
//! # Three gates, and what each of them does *not* prevent
//!
//! This is the part worth reading before trusting it, because a policy that is believed to do
//! more than it does is worse than no policy.
//!
//! | gate | what it decides | what it cannot decide |
//! |---|---|---|
//! | [`allow_host_suffix`](NetPolicy::allow_host_suffix) | which DNS names may be **looked up** | nothing about the addresses that come back |
//! | [`allow_port`](NetPolicy::allow_port) | which destination **ports** a connect or a sendto may use | which host is on the far end of one |
//! | [`allow_loopback`](NetPolicy::allow_loopback) | whether destinations that never leave the machine are reachable at all | nothing about what is listening on them |
//!
//! **The host gate is a resolver gate, and it stops at the resolver.** A name leaves this machine
//! the moment it is looked up, so refusing to look one up is a real and checkable restriction. But
//! once `clientsettingscdn.roblox.com` has resolved to an address, that address is an address:
//! nothing in a `connect(2)` carries the name it came from, and this policy has no memory of which
//! answers it gave. So a guest that has an address by some other route — a literal in its own
//! data, a redirect, a DNS answer it obtained before the policy was narrowed — reaches it if the
//! port gate admits the port.
//!
//! Closing that hole needs a **resolver that remembers its answers** and an address gate that
//! admits only addresses this instance's own resolutions produced. That is a real design and it is
//! deliberately not built here, because it has a failure mode of its own (a TTL expiring under a
//! live connection, and a CDN that answers differently per query) and nothing has yet measured
//! whether the engine needs it. What is written down instead is the *limit*, so that nobody reads
//! `allow_host_suffix("roblox.com")` as a guarantee that only Roblox is reachable. It is not; it
//! is a guarantee that only Roblox names are asked about.
//!
//! # Suffix matching, and the near-miss it has to refuse
//!
//! `allow_host_suffix("roblox.com")` admits `roblox.com` and `clientsettingscdn.roblox.com`. It
//! must **not** admit `notroblox.com` or `roblox.com.attacker.example`, and a naive
//! `host.ends_with(suffix)` admits the first of those. The rule implemented here is *equal to the
//! suffix, or ends with a dot followed by the suffix*, which is the label-boundary rule every
//! cookie and certificate implementation has had to learn. It is tested on the near miss rather
//! than on the match, because a match passes against the naive version too.

use std::collections::BTreeSet;

use super::address::SocketAddress;
use super::error::{NetError, NetResult};

/// Which destinations one guest instance may reach.
///
/// Cheap to clone and cheap to share: an embedding builds one and hands an [`Arc`] of it to every
/// socket the instance creates.
///
/// [`Arc`]: std::sync::Arc
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetPolicy {
    /// Every destination is allowed. Set only by [`NetPolicy::unrestricted`].
    unrestricted: bool,
    /// Destinations that never leave the machine are allowed, on any port.
    loopback: bool,
    /// DNS name suffixes that may be looked up, lowercase, without a leading dot.
    host_suffixes: BTreeSet<String>,
    /// Destination ports a connect or a sendto may use, for anything that is not loopback.
    ports: BTreeSet<u16>,
}

impl NetPolicy {
    /// The default: **nothing is reachable and nothing may be looked up**.
    ///
    /// The successor to Global Constraint 8's blanket refusal, and the state an embedding that has
    /// not decided anything is in. Every refusal it produces names this type and says what would
    /// change it, because a refusal that does not is indistinguishable from a bug.
    #[must_use]
    pub fn closed() -> NetPolicy {
        NetPolicy::default()
    }

    /// Only destinations that never leave the machine, on any port.
    ///
    /// **What the test suite runs on**, and the reason it is a named constructor rather than
    /// `closed().allow_loopback()`: a loopback-only policy is a meaningful thing for an embedding
    /// to choose — a guest that talks to a local proxy and nothing else — and a name says so where
    /// a builder chain reads as an accident.
    #[must_use]
    pub fn loopback_only() -> NetPolicy {
        NetPolicy { loopback: true, ..NetPolicy::default() }
    }

    /// Every destination, every port, every name.
    ///
    /// **Named so that it cannot be reached by accident.** There is no `NetPolicy::new()` that
    /// happens to be this, no builder that becomes this once enough rules are added, and no flag
    /// that flips into it. An embedding that wants an unrestricted guest has to write the word,
    /// and D6's threat — a cheat-injected APK carrying a Luau executor — is what the word is
    /// worth reading twice for.
    #[must_use]
    pub fn unrestricted() -> NetPolicy {
        NetPolicy { unrestricted: true, ..NetPolicy::default() }
    }

    /// Allow destinations that never leave the machine.
    #[must_use]
    pub fn allow_loopback(mut self) -> NetPolicy {
        self.loopback = true;
        self
    }

    /// Allow a DNS name and everything under it to be looked up.
    ///
    /// `allow_host_suffix("roblox.com")` admits `roblox.com` and `api.roblox.com`, and refuses
    /// `notroblox.com`. Matching is case-insensitive and a trailing dot on either side is ignored,
    /// because `roblox.com.` is the same name written absolutely.
    ///
    /// An empty suffix is ignored rather than admitted: `""` is a suffix of every name, so
    /// accepting it would silently turn a host list into no host list at all.
    #[must_use]
    pub fn allow_host_suffix(mut self, suffix: &str) -> NetPolicy {
        let normalised = normalise_name(suffix);
        if !normalised.is_empty() {
            self.host_suffixes.insert(normalised);
        }
        self
    }

    /// Allow a destination port.
    ///
    /// Applies to anything that is not loopback. Roblox needs 443 for settings, auth and API
    /// (D30 records `https://clientsettingscdn.roblox.com/v2/settings/application/android` as the
    /// measured first request), and the game protocol's UDP ports besides.
    #[must_use]
    pub fn allow_port(mut self, port: u16) -> NetPolicy {
        self.ports.insert(port);
        self
    }

    /// Allow every port in a range, endpoints included.
    ///
    /// The UDP game protocol picks a port out of a block rather than using one, so listing them
    /// one at a time would be a hundred builder calls at a call site that should read as one rule.
    #[must_use]
    pub fn allow_port_range(mut self, first: u16, last: u16) -> NetPolicy {
        for port in first..=last {
            self.ports.insert(port);
        }
        self
    }

    /// Whether this policy admits nothing at all.
    ///
    /// Diagnostic: an embedding can assert it before a run, and a refusal message uses it to say
    /// *nothing has been opened* rather than *this particular destination is not on the list*,
    /// which are different problems for whoever is reading the log.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        !self.unrestricted && !self.loopback && self.ports.is_empty()
    }

    /// The rules, rendered for a refusal message.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.unrestricted {
            return "every destination (NetPolicy::unrestricted)".to_owned();
        }
        if self.is_closed() && self.host_suffixes.is_empty() {
            return "nothing at all (NetPolicy::closed, the default)".to_owned();
        }
        let mut parts = Vec::new();
        if self.loopback {
            parts.push("loopback on any port".to_owned());
        }
        if !self.ports.is_empty() {
            let ports: Vec<String> = self.ports.iter().map(u16::to_string).collect();
            parts.push(format!("ports {}", ports.join(", ")));
        }
        if !self.host_suffixes.is_empty() {
            let hosts: Vec<&str> = self.host_suffixes.iter().map(String::as_str).collect();
            parts.push(format!("names under {}", hosts.join(", ")));
        }
        parts.join("; ")
    }

    /// May this instance send to this address?
    ///
    /// Asked by `connect` and by every `sendto`, because a datagram carries its own destination
    /// and a policy checked only at connect would not see one.
    ///
    /// # Errors
    ///
    /// [`NetError::Policy`] naming the destination, the rule that refused it, and what the policy
    /// currently admits.
    pub fn check_address(
        &self,
        operation: &'static str,
        address: &SocketAddress,
    ) -> NetResult<()> {
        if self.unrestricted {
            return Ok(());
        }
        if address.is_loopback() {
            if self.loopback {
                return Ok(());
            }
            return Err(NetError::policy(
                operation,
                address.to_string(),
                format!(
                    "the destination is loopback and this instance's policy does not allow \
                     loopback. It allows: {}",
                    self.describe()
                ),
            ));
        }
        if self.ports.contains(&address.port()) {
            return Ok(());
        }
        Err(NetError::policy(
            operation,
            address.to_string(),
            format!(
                "port {} is not one this instance's policy allows. It allows: {}",
                address.port(),
                self.describe()
            ),
        ))
    }

    /// May this instance look this name up?
    ///
    /// Asked by [`resolve`](super::resolve) before any query leaves the machine. The port is
    /// checked here too, so that a lookup for a destination the policy could never connect to is
    /// refused before the name is sent to a resolver rather than after.
    ///
    /// # Errors
    ///
    /// [`NetError::Policy`] naming the host, the rule that refused it, and what the policy
    /// currently admits.
    pub fn check_host(&self, operation: &'static str, host: &str, port: u16) -> NetResult<()> {
        if self.unrestricted {
            return Ok(());
        }
        let name = normalise_name(host);
        // A literal address is not a name: nothing is looked up, so the host gate has nothing to
        // say about it and the address gate is the whole of the decision. Parsing it here rather
        // than leaving it to the resolver means `getaddrinfo("127.0.0.1", …)` under a
        // loopback-only policy behaves the same way `connect(127.0.0.1)` does, which is the
        // agreement a caller would otherwise discover the hard way.
        if let Ok(literal) = name.parse::<std::net::IpAddr>() {
            let address = SocketAddress::from_std(std::net::SocketAddr::new(literal, port));
            return self.check_address(operation, &address);
        }
        // `localhost` is a name for a loopback address on every host this runtime targets, and it
        // is the name a local proxy is configured with. It is admitted by the loopback gate rather
        // than by the host list, for the same reason the literal above is.
        if self.loopback && (name == "localhost" || name.ends_with(".localhost")) {
            return Ok(());
        }
        if !self.host_suffixes.iter().any(|suffix| matches_suffix(&name, suffix)) {
            return Err(NetError::policy(
                operation,
                format!("{host}:{port}"),
                format!(
                    "`{host}` is not under any name suffix this instance's policy allows, so the \
                     query is refused before it leaves this machine. It allows: {}",
                    self.describe()
                ),
            ));
        }
        if !self.ports.contains(&port) {
            return Err(NetError::policy(
                operation,
                format!("{host}:{port}"),
                format!(
                    "the name `{host}` is allowed and port {port} is not, so a lookup for it \
                     would name a destination this policy could never connect to. It allows: {}",
                    self.describe()
                ),
            ));
        }
        Ok(())
    }
}

/// Lowercase a name and drop a trailing dot, so that two spellings of one name compare equal.
///
/// The trailing dot is the absolute form — `roblox.com.` is the same name as `roblox.com` — and a
/// guest that writes one where the policy writes the other must not get a different answer.
fn normalise_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Does `name` sit at or under `suffix`, on a label boundary?
///
/// **The whole of the near-miss defence.** `name.ends_with(suffix)` admits `notroblox.com` for the
/// suffix `roblox.com`, which is a different registrable domain owned by somebody else. The rule
/// is *equal, or ends with a dot then the suffix*, and the test for it is written on the near miss
/// rather than on the match, because the match passes against the wrong version too.
fn matches_suffix(name: &str, suffix: &str) -> bool {
    if name == suffix {
        return true;
    }
    name.len() > suffix.len()
        && name.ends_with(suffix)
        && name.as_bytes()[name.len() - suffix.len() - 1] == b'.'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::address::IpFamily;

    /// The default refuses everything, and says so by naming the type that would change it.
    #[test]
    fn the_default_policy_refuses_every_destination_and_names_what_would_open_it() {
        let policy = NetPolicy::default();
        assert_eq!(policy, NetPolicy::closed());
        assert!(policy.is_closed());
        for address in [
            SocketAddress::loopback(IpFamily::V4, 443),
            SocketAddress::loopback(IpFamily::V6, 443),
            SocketAddress::V4 { address: [93, 184, 216, 34], port: 443 },
        ] {
            let err = policy.check_address("connect", &address).unwrap_err();
            let text = err.to_string();
            assert!(text.contains("NetPolicy"), "the refusal must name the seam: {text}");
            assert!(text.contains(&address.to_string()), "{text}");
            assert_eq!(err.kind(), None, "a policy refusal is not an errno");
        }
        let err = policy.check_host("getaddrinfo", "clientsettingscdn.roblox.com", 443).unwrap_err();
        assert!(err.to_string().contains("nothing at all"), "{err}");
    }

    /// A loopback-only policy admits loopback on any port and nothing else.
    #[test]
    fn a_loopback_only_policy_admits_loopback_on_any_port_and_refuses_the_outside() {
        let policy = NetPolicy::loopback_only();
        assert!(!policy.is_closed());
        for port in [1_u16, 443, 49_152, 65_535] {
            policy
                .check_address("connect", &SocketAddress::loopback(IpFamily::V4, port))
                .expect("loopback on any port");
            policy
                .check_address("connect", &SocketAddress::loopback(IpFamily::V6, port))
                .expect("v6 loopback on any port");
        }
        let outside = SocketAddress::V4 { address: [93, 184, 216, 34], port: 443 };
        let err = policy.check_address("connect", &outside).unwrap_err();
        assert!(err.to_string().contains("port 443 is not one"), "{err}");
        // `localhost` resolves to loopback, so it is admitted; a real name is not.
        policy.check_host("getaddrinfo", "localhost", 8080).expect("localhost is loopback");
        assert!(policy.check_host("getaddrinfo", "roblox.com", 443).is_err());
    }

    /// The suffix rule refuses the near miss, which is the case a match-only test never reaches.
    #[test]
    fn a_host_suffix_matches_on_a_label_boundary_and_not_on_a_substring() {
        let policy = NetPolicy::closed().allow_host_suffix("roblox.com").allow_port(443);
        for admitted in [
            "roblox.com",
            "ROBLOX.COM",
            "roblox.com.",
            "clientsettingscdn.roblox.com",
            "apis.roblox.com",
        ] {
            policy
                .check_host("getaddrinfo", admitted, 443)
                .unwrap_or_else(|e| panic!("`{admitted}` should be allowed: {e}"));
        }
        for refused in [
            // The near miss: `ends_with("roblox.com")` is true and the registrable domain differs.
            "notroblox.com",
            "evilroblox.com",
            // The suffix as a prefix of somebody else's name.
            "roblox.com.attacker.example",
            "example.com",
        ] {
            let err = policy
                .check_host("getaddrinfo", refused, 443)
                .unwrap_err();
            assert!(err.to_string().contains(refused), "{err}");
        }
    }

    /// An empty suffix is ignored, because it would otherwise admit every name silently.
    #[test]
    fn an_empty_host_suffix_does_not_become_a_wildcard() {
        let policy = NetPolicy::closed().allow_host_suffix("").allow_port(443);
        assert!(
            policy.check_host("getaddrinfo", "anything.example", 443).is_err(),
            "an empty suffix must not turn a host list into no host list"
        );
    }

    /// An allowed name on a port the policy does not carry is refused before the query leaves.
    #[test]
    fn an_allowed_name_on_a_refused_port_is_stopped_before_the_resolver_is_asked() {
        let policy = NetPolicy::closed().allow_host_suffix("roblox.com").allow_port(443);
        let err = policy.check_host("getaddrinfo", "roblox.com", 8080).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("port 8080 is not"), "{text}");
        assert!(text.contains("could never connect"), "{text}");
    }

    /// A literal address given where a name is expected is decided by the address gate.
    ///
    /// The agreement matters: a caller that writes `getaddrinfo("127.0.0.1", "443")` and one that
    /// writes `connect(127.0.0.1:443)` are asking for the same destination, and a policy that
    /// answered differently would be a rule nobody could state.
    #[test]
    fn an_address_literal_is_judged_as_an_address_rather_than_as_a_name() {
        let loopback = NetPolicy::loopback_only();
        loopback.check_host("getaddrinfo", "127.0.0.1", 9_000).expect("a loopback literal");
        loopback.check_host("getaddrinfo", "::1", 9_000).expect("a v6 loopback literal");
        assert!(loopback.check_host("getaddrinfo", "93.184.216.34", 443).is_err());

        let outward = NetPolicy::closed().allow_port(443);
        outward.check_host("getaddrinfo", "93.184.216.34", 443).expect("an allowed port");
        assert!(
            outward.check_host("getaddrinfo", "93.184.216.34", 8080).is_err(),
            "the address gate is the port gate for a literal"
        );
    }

    /// `unrestricted` admits everything, and is the only thing that does.
    #[test]
    fn unrestricted_admits_everything_and_is_reachable_only_by_name() {
        let policy = NetPolicy::unrestricted();
        assert!(!policy.is_closed());
        policy
            .check_address("connect", &SocketAddress::V4 { address: [1, 1, 1, 1], port: 53 })
            .expect("unrestricted");
        policy.check_host("getaddrinfo", "anything.example", 1).expect("unrestricted");
        assert!(policy.describe().contains("unrestricted"), "{}", policy.describe());
        // No chain of the ordinary builders reaches it.
        let built = NetPolicy::closed()
            .allow_loopback()
            .allow_host_suffix("example.com")
            .allow_port_range(0, 65_535);
        assert_ne!(built, policy, "a builder chain must not become `unrestricted`");
        assert!(built.check_host("getaddrinfo", "anything.example", 1).is_err());
    }

    /// A port range admits its endpoints, which is where an exclusive range would be wrong.
    #[test]
    fn a_port_range_includes_both_of_its_endpoints() {
        let policy = NetPolicy::closed().allow_port_range(49_152, 49_154);
        for port in [49_152_u16, 49_153, 49_154] {
            policy
                .check_address("sendto", &SocketAddress::V4 { address: [1, 1, 1, 1], port })
                .unwrap_or_else(|e| panic!("port {port} should be allowed: {e}"));
        }
        assert!(policy
            .check_address("sendto", &SocketAddress::V4 { address: [1, 1, 1, 1], port: 49_155 })
            .is_err());
    }
}
