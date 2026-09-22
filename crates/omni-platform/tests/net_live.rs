//! Tests that reach the **real internet**, and therefore need one.
//!
//! ```text
//! OMNI_NET_TESTS=1 cargo test -p omni-platform --test net_live -- --ignored --test-threads=1
//! ```
//!
//! # How this file obeys VERIFICATION entry 4 without breaking an offline build
//!
//! Entry 4 is the rule that a test which cannot run must **fail**, not skip: a test that
//! early-returns when its fixture is missing reports `ok` and proves nothing, and that is how two
//! of the filesystem seam's six confinement rules came to have never executed on the machine whose
//! green suite was the evidence for them.
//!
//! A name cannot be resolved and a connection cannot be made without a working network, and a
//! build machine may not have one. So the two states are made *visibly different* rather than
//! reconciled, exactly as `window_live.rs` and `omni-gfx`'s `renderer_live.rs` do:
//!
//! * **Not asked for.** Every test here is `#[ignore]`d with a reason that names the environment
//!   variable. An ordinary `cargo test --workspace --release` reports them as `ignored`, which is
//!   a line of output saying they did not run — not an `ok` claiming they did.
//! * **Asked for.** `cargo test -- --ignored` means someone decided these should run. If
//!   `OMNI_NET_TESTS` is not set to 1, they **panic** naming the variable, because at that point a
//!   silent skip would be exactly entry 4's failure.
//!
//! What this cannot do is notice a host that has a network and is behind a proxy that answers
//! everything. Nothing can; that is why the gate is an explicit decision by whoever runs the suite
//! rather than a probe.
//!
//! # Why the opt-in is a decision and not a convenience
//!
//! It is not only about flakiness. D30 withdrew Global Constraint 8 and replaced it with a policy,
//! and the whole argument for that shape is that **reaching the network is something somebody
//! decides**. A test file that silently opened a connection to a Roblox endpoint every time
//! anybody typed `cargo test` would be the first thing in this workspace to make that decision on
//! the operator's behalf. The variable is the operator making it.
//!
//! # What these assert, and what they deliberately do not
//!
//! They assert that a name resolves to at least one address, that a non-blocking connect to it
//! settles as *connected*, and that the socket is then writable. They do **not** speak TLS, send
//! an HTTP request or look at a response: `libroblox.so` carries its own OpenSSL (D30), so this
//! layer owes it sockets and names and not one line of cryptography, and a test that did TLS here
//! would be testing a second implementation that the guest will never use.

use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_platform::net::{
    poll, resolve, ConnectOutcome, Interest, IpFamily, NetPolicy, PollEntry, ResolveFailure,
    Socket, SocketKind,
};

/// The opt-in. Named for the network bring-up rather than for one test, because every test in this
/// file is gated on the same decision by the same person.
const GATE: &str = "OMNI_NET_TESTS";

/// The name D30 records the engine asking for, measured: the first thing `libroblox.so` reaches
/// for once it has flags to load.
const MEASURED_HOST: &str = "clientsettingscdn.roblox.com";

/// Fail — loudly, naming the variable — if these were run without the opt-in.
///
/// Reaching this function at all means `--ignored` was passed, i.e. somebody asked for the network
/// tests. Answering that request with a silent success is the defect VERIFICATION entry 4 records.
fn require_gate() {
    let set = std::env::var(GATE).is_ok_and(|value| value == "1");
    assert!(
        set,
        "this test was run with --ignored but {GATE} is not set to 1. It resolves a real name and \
         opens a real connection to the internet; it will not pretend to have passed without \
         one. Set {GATE}=1 to run it, or drop --ignored to skip it visibly."
    );
}

/// The policy these tests run under: exactly the destination D30 measured, and nothing else.
///
/// Deliberately **not** [`NetPolicy::unrestricted`]. If one of these tests were written wrongly —
/// a typo in a host name, a port from a different service — the policy refuses it before a packet
/// leaves, and the failure names the rule rather than producing a connection nobody intended.
fn measured_policy() -> Arc<NetPolicy> {
    Arc::new(NetPolicy::closed().allow_host_suffix("roblox.com").allow_port(443))
}

/// The name the engine asks for resolves to at least one address.
#[test]
#[ignore = "reaches the real internet; set OMNI_NET_TESTS=1 and run with --ignored"]
fn the_name_the_engine_asks_for_resolves() {
    require_gate();
    let policy = measured_policy();
    let addresses = resolve(MEASURED_HOST, 443, None, &policy).unwrap_or_else(|e| {
        panic!("resolving {MEASURED_HOST}: {e}");
    });
    assert!(!addresses.is_empty(), "a successful resolution with no addresses");
    for address in &addresses {
        assert_eq!(address.port(), 443, "{address} does not carry the port that was asked for");
        assert!(!address.is_loopback(), "{address} is loopback, which is not a CDN");
        assert!(!address.is_unspecified(), "{address} is the wildcard address");
    }
}

/// A name that does not exist is `NoSuchHost` and is **not** retryable.
///
/// `.invalid` is reserved by RFC 2606 precisely so that it can never resolve, which is what makes
/// this a test of the classification rather than of somebody's DNS.
///
/// It is the one live test whose failure mode is interesting on its own: a resolver behind a
/// captive portal or an ISP that answers every name with an ad server will return an *address*
/// here, and this test is where that host announces itself.
#[test]
#[ignore = "reaches the real internet; set OMNI_NET_TESTS=1 and run with --ignored"]
fn a_name_that_cannot_exist_is_classified_as_permanent_rather_than_transient() {
    require_gate();
    // `unrestricted` here and nowhere else in this file: the point is to ask a resolver about a
    // name no policy would sensibly list, so the policy cannot be what refuses it.
    let policy = NetPolicy::unrestricted();
    let err = resolve("omnidroid-no-such-name.invalid", 443, None, &policy).expect_err(
        "a .invalid name resolved, which means this host's resolver answers names that do not \
         exist — every name-based test on this machine is meaningless until that is fixed",
    );
    let failure = err.resolve_failure().unwrap_or_else(|| panic!("not a resolver failure: {err}"));
    assert_eq!(failure, ResolveFailure::NoSuchHost, "{err}");
    assert!(
        !failure.is_retryable(),
        "a name that does not exist must not be retried: that is how a client spins"
    );
}

/// A non-blocking connect to the measured endpoint settles as connected.
///
/// The sequence is the one a real client uses and the one the engine will: resolve, create a
/// non-blocking socket, start the connect, wait for writability, read `SO_ERROR`. Nothing here
/// speaks TLS — the guest's own OpenSSL does that.
#[test]
#[ignore = "reaches the real internet; set OMNI_NET_TESTS=1 and run with --ignored"]
fn a_nonblocking_connect_to_the_measured_endpoint_completes() {
    require_gate();
    let policy = measured_policy();
    let addresses = resolve(MEASURED_HOST, 443, Some(IpFamily::V4), &policy)
        .unwrap_or_else(|e| panic!("resolving {MEASURED_HOST} for IPv4: {e}"));
    let address = addresses[0];

    let mut client = Socket::new(SocketKind::Stream, IpFamily::V4, Arc::clone(&policy))
        .expect("a stream socket");
    client.set_nonblocking(true).expect("non-blocking mode");
    let progress = client.connect(&address).unwrap_or_else(|e| panic!("connect to {address}: {e}"));
    println!("connect to {address} answered {progress:?}");

    // Ten seconds: this bounds *failure* only, and a passing run leaves as soon as the handshake
    // finishes. A real internet path is slower than loopback and the bound has to allow for it.
    let deadline = Instant::now() + Duration::from_secs(10);
    let outcome = loop {
        let mut entries = [PollEntry::new(&client, Interest::WRITABLE)];
        poll(&mut entries, Duration::from_millis(100)).expect("readiness");
        let readiness = entries[0].readiness();
        if readiness.writable || readiness.error {
            break client.connect_result().expect("the connect result");
        }
        assert!(
            Instant::now() < deadline,
            "the connect to {address} never settled; the socket reported {readiness:?}"
        );
    };
    assert_eq!(outcome, ConnectOutcome::Connected, "connecting to {address}");

    assert_eq!(client.peer_address().expect("getpeername"), address);
    assert!(
        client.readiness().expect("readiness").writable,
        "a connected socket with an empty send buffer is writable"
    );
}

/// The policy refuses a destination it does not cover, even with a working network behind it.
///
/// The counterpart of the test above, and the reason it is here rather than in `net_seam.rs`: on a
/// machine that genuinely could reach `example.com`, the thing that stops it is the embedding's
/// rule and not the absence of a route.
#[test]
#[ignore = "reaches the real internet; set OMNI_NET_TESTS=1 and run with --ignored"]
fn the_policy_refuses_a_name_it_does_not_cover_on_a_machine_that_could_reach_it() {
    require_gate();
    let policy = measured_policy();
    // First prove this host could answer the question, so the refusal below is the policy's doing.
    resolve("example.com", 443, None, &NetPolicy::unrestricted())
        .expect("example.com must resolve on a machine running the live network tests");
    let err = resolve("example.com", 443, None, &policy).unwrap_err();
    assert_eq!(err.resolve_failure(), None, "a policy refusal is not a resolver failure");
    assert!(err.to_string().contains("example.com"), "{err}");
}
