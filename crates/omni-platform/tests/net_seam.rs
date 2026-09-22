//! Tests of the platform-independent half of the network seam: the policy, the address type, the
//! diagnostic quality of the errors, and the resolution that needs no resolver.
//!
//! **These run on every target and need no socket.** Everything that needs a real one — and
//! therefore the Windows backend, which is the only one that exists — is in `net_loopback.rs`.
//! Everything that needs the internet is in `net_live.rs`, gated and `#[ignore]`d.
//!
//! The split exists because VERIFICATION entry 4 is about a test that skipped when its fixture was
//! missing and reported `ok` anyway. A file that could quietly become a no-op on a host with no
//! backend is the same shape, so the three halves are three files: this one has no fixture to be
//! missing, `net_loopback.rs` is compiled only where its fixture exists, and `net_live.rs`
//! **fails** when it is asked to run without one.

use std::sync::Arc;

use omni_platform::net::{
    resolve, service_port, IpFamily, NetError, NetErrorKind, NetPolicy, ResolveFailure,
    SocketAddress, IMPLEMENTED_OPTIONS, MAX_POLL_SOCKETS,
};

/// The default policy is closed, and a closed policy's refusal says what would open it.
///
/// This is D30's replacement for Global Constraint 8 asserted as a *property of the default*
/// rather than of a configuration: an embedding that has not thought about the network gets a
/// runtime that reaches nothing, and it gets it without anybody remembering to write a check.
#[test]
fn an_embedding_that_decides_nothing_gets_a_guest_that_reaches_nothing() {
    let policy = NetPolicy::default();
    assert!(policy.is_closed());
    assert_eq!(policy, NetPolicy::closed());

    let destination = SocketAddress::V4 { address: [93, 184, 216, 34], port: 443 };
    let err = policy.check_address("connect", &destination).unwrap_err();
    assert!(matches!(err, NetError::Policy { .. }), "{err}");
    let text = err.to_string();
    assert!(text.contains("93.184.216.34:443"), "the refusal names the destination: {text}");
    assert!(text.contains("NetPolicy"), "and names what would open it: {text}");
    assert!(text.contains("D30"), "and cites the decision that made it a policy: {text}");
    assert_eq!(
        err.kind(),
        None,
        "a policy refusal must not become an errno: ENETUNREACH would hide a configuration fact \
         in the ordinary noise of a client failing over"
    );
}

/// A policy that names a host suffix does not admit a different registrable domain.
///
/// Asserted on the **near miss** rather than on the match, because `host.ends_with(suffix)` — the
/// wrong implementation — passes every test built from names that are genuinely under the suffix.
#[test]
fn a_host_suffix_policy_refuses_the_name_that_merely_ends_with_it() {
    let policy = NetPolicy::closed().allow_host_suffix("roblox.com").allow_port(443);
    policy
        .check_host("getaddrinfo", "clientsettingscdn.roblox.com", 443)
        .expect("the name D30 records the engine asking for");
    for impostor in ["notroblox.com", "xroblox.com", "roblox.com.attacker.example"] {
        let err = policy.check_host("getaddrinfo", impostor, 443).unwrap_err();
        assert!(err.to_string().contains(impostor), "{err}");
    }
}

/// Resolution consults the policy before anything leaves the machine.
#[test]
fn a_lookup_outside_the_policy_never_reaches_a_resolver() {
    let err = resolve("clientsettingscdn.roblox.com", 443, None, &NetPolicy::closed()).unwrap_err();
    assert!(matches!(err, NetError::Policy { .. }), "{err}");
    assert_eq!(err.resolve_failure(), None, "a policy refusal is not a resolver failure");
}

/// An address literal resolves without a resolver, on every target.
///
/// The one resolution that can be asserted with no network at all: `std` parses a literal itself.
#[test]
fn an_address_literal_resolves_and_keeps_its_port() {
    let policy = NetPolicy::loopback_only();
    let v4 = resolve("127.0.0.1", 8_080, None, &policy).expect("a v4 literal");
    assert_eq!(v4, vec![SocketAddress::V4 { address: [127, 0, 0, 1], port: 8_080 }]);
    let v6 = resolve("::1", 8_080, Some(IpFamily::V6), &policy).expect("a v6 literal");
    assert_eq!(v6, vec![SocketAddress::loopback(IpFamily::V6, 8_080)]);
}

/// The three resolution failures a caller behaves differently about stay apart.
///
/// The one that matters is `is_retryable`: a client that retries `EAI_NONAME` spins for ever, and
/// one that gives up on `EAI_AGAIN` fails a session a second attempt would have completed.
#[test]
fn the_resolution_failures_a_caller_acts_on_are_not_interchangeable() {
    assert!(ResolveFailure::Transient.is_retryable());
    for permanent in [
        ResolveFailure::NoSuchHost,
        ResolveFailure::NoAddressOfFamily,
        ResolveFailure::NonRecoverable,
        ResolveFailure::Unclassified,
    ] {
        assert!(!permanent.is_retryable(), "{permanent} must not be retried");
    }

    // Asking a v4 literal for its v6 addresses is "no address of that family", not "no such
    // name" — derived here rather than reported by any host, so it holds on all five targets.
    let err = resolve("127.0.0.1", 443, Some(IpFamily::V6), &NetPolicy::loopback_only())
        .unwrap_err();
    assert_eq!(err.resolve_failure(), Some(ResolveFailure::NoAddressOfFamily));
}

/// A service *name* is refused by name; a numeric service is not.
#[test]
fn a_service_name_is_refused_rather_than_guessed_at() {
    assert_eq!(service_port("443").unwrap(), 443);
    let err = service_port("https").unwrap_err();
    let text = err.to_string();
    assert!(text.contains("getservbyname(3)"), "the refusal names the missing work: {text}");
    assert!(
        !text.contains("443"),
        "the refusal must not name the port it declined to guess: {text}"
    );
}

/// An unimplemented socket option is refused by its own numbers and says what is available.
///
/// This is rule 1's shape exactly: a `setsockopt` the caller believes took effect is worse than
/// one that failed, so the adapter has a constructor for the refusal and no way to return success.
#[test]
fn an_option_this_seam_does_not_implement_is_refused_by_number() {
    // SOL_SOCKET = 1 and SO_LINGER = 13 in the *guest's* numbering (Linux arm64). This crate does
    // not own that table, which is why the constructor takes the numbers rather than an enum.
    let err = NetError::unimplemented_option("setsockopt", 1, 13);
    let text = err.to_string();
    assert!(text.contains("option 13"), "{text}");
    assert!(text.contains("level 1"), "{text}");
    assert!(text.contains(IMPLEMENTED_OPTIONS), "{text}");
    assert!(text.contains("silently ignored"), "the message says why it is not accepted: {text}");
}

/// The address type round-trips through `std` without dropping the two IPv6 fields.
///
/// `scope_id` is what makes a link-local address usable, and a fixture built from zeros passes
/// against an implementation that drops it — VERIFICATION entry 1's shape, so the fixture is
/// non-zero in every field.
#[test]
fn a_v6_endpoint_survives_the_round_trip_the_adapter_will_marshal_it_through() {
    let original = SocketAddress::V6 {
        address: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0x02, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55],
        port: 49_152,
        flowinfo: 0x000a_bcde,
        scope_id: 17,
    };
    assert_eq!(SocketAddress::from_std(original.to_std()), original);
    assert_eq!(original.family(), IpFamily::V6);
    assert_eq!(original.port(), 49_152);
    assert_eq!(original.address_bytes().len(), 16);
}

/// Every error kind has a distinct rendering, so no message is ambiguous about what happened.
#[test]
fn the_error_kinds_do_not_render_the_same_way_as_one_another() {
    let kinds = [
        NetErrorKind::WouldBlock,
        NetErrorKind::InProgress,
        NetErrorKind::ConnectionRefused,
        NetErrorKind::NotConnected,
        NetErrorKind::TimedOut,
        NetErrorKind::Other,
    ];
    let mut names: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), kinds.len());
    assert_ne!(
        NetErrorKind::WouldBlock.as_str(),
        NetErrorKind::InProgress.as_str(),
        "a pending connect and a full send buffer are different answers"
    );
}

/// The poll limit is a number the public API states, not one a caller discovers by overflowing it.
#[test]
fn the_poll_limit_is_public_and_is_the_size_of_the_set_it_protects() {
    assert_eq!(MAX_POLL_SOCKETS, 64, "FD_SETSIZE on the one tested host");
}

/// A policy is shareable, which is how one instance's rules reach every socket it makes.
#[test]
fn a_policy_is_shared_by_reference_rather_than_copied_per_socket() {
    let policy = Arc::new(NetPolicy::loopback_only());
    let second = Arc::clone(&policy);
    assert_eq!(Arc::strong_count(&policy), 2);
    assert_eq!(*second, *policy);
}
