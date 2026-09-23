//! Real sockets, over loopback: the backend half of the network seam, exercised end to end.
//!
//! ```text
//! cargo test -p omni-platform --test net_loopback
//! ```
//!
//! # Why loopback and not a mock, and why these are not `#[ignore]`d
//!
//! Every test here creates a real socket, makes a real non-blocking `connect`, and moves real
//! bytes. The peer is `std::net`'s own listener or datagram socket in the same process, so
//! **nothing leaves the machine** — there is no DNS server to be down, no port to be firewalled
//! and no network to be flaky. A loopback interface exists on every host that can run `cargo
//! test` at all, so there is no fixture to be missing and therefore nothing for VERIFICATION
//! entry 4 to be about: these run in the ordinary suite, every time.
//!
//! What genuinely needs the internet — a name that has to be resolved, a TLS endpoint that has to
//! answer — is in `net_live.rs`, `#[ignore]`d and gated, and it **panics** rather than passing
//! when it is asked to run without the opt-in.
//!
//! # This file is compiled only where a backend exists
//!
//! `#![cfg(target_os = "windows")]`, matching `vm_windows.rs`. The Linux and macOS backends are
//! structural — every call returns `Unsupported` naming the POSIX call it intends to make — so
//! `Socket::new` cannot produce a socket there and none of these tests has anything to assert.
//! Compiling them into an empty file on those targets is the honest state: the seam has not been
//! implemented there, and a test that "passed" by asserting the refusal would be asserting the
//! absence of an implementation rather than the presence of one.
//!
//! # The policy every test runs under
//!
//! [`NetPolicy::loopback_only`], which is the whole point of the seam being a policy rather than
//! an open socket: this file cannot accidentally reach the internet even if a test were written
//! wrongly, because the policy would refuse the address before any packet left.
#![cfg(target_os = "windows")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream as HostStream, UdpSocket as HostDatagram};
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_platform::net::{
    poll, ConnectOutcome, ConnectProgress, Interest, IpFamily, NetError, NetErrorKind, NetPolicy,
    OptionValue, PollEntry, Shutdown, Socket, SocketAddress, SocketKind, SocketOption,
    SocketQuery, MAX_POLL_SOCKETS,
};

/// How long a loopback operation is given before the test calls it a failure.
///
/// Generous, because it only bounds *failure*: a passing run leaves the moment the thing it is
/// waiting for happens. VERIFICATION entry 6 is the record of four flakes fixed by turning a
/// sleep into a bounded poll on the thing under test, and this is that shape.
const DEADLINE: Duration = Duration::from_secs(5);

/// The policy this file runs under: loopback on any port, and nothing else.
fn loopback_policy() -> Arc<NetPolicy> {
    Arc::new(NetPolicy::loopback_only())
}

/// A non-blocking socket of the given kind and family.
fn socket(kind: SocketKind, family: IpFamily) -> Socket {
    let mut socket = Socket::new(kind, family, loopback_policy())
        .unwrap_or_else(|e| panic!("creating a {family} {kind:?} socket: {e}"));
    socket.set_nonblocking(true).expect("non-blocking mode");
    socket
}

/// Wait until `want` is true of the socket's readiness, or the deadline passes.
///
/// A bounded poll on the thing under test rather than a sleep-and-hope. Returns the readiness it
/// stopped on, so a failing assertion can say what it *did* see.
fn wait_until(
    socket: &Socket,
    interest: Interest,
    what: &str,
    want: impl Fn(omni_platform::net::Readiness) -> bool,
) -> omni_platform::net::Readiness {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let mut entries = [PollEntry::new(socket, interest)];
        poll(&mut entries, Duration::from_millis(50)).expect("a readiness call over loopback");
        let readiness = entries[0].readiness();
        if want(readiness) {
            return readiness;
        }
        assert!(
            Instant::now() < deadline,
            "waited {DEADLINE:?} for {what} and the socket reported {readiness:?}"
        );
    }
}

/// A loopback address whose port has a listener behind it, and the listener.
fn listening(family: IpFamily) -> (TcpListener, SocketAddress) {
    let bind = SocketAddress::loopback(family, 0);
    let listener = TcpListener::bind(bind.to_std()).expect("a loopback listener");
    let address = SocketAddress::from_std(listener.local_addr().expect("the listener's address"));
    (listener, address)
}

/// A loopback address with **nothing** behind it.
///
/// Bound and dropped, so the port was free a moment ago and is free now. This is the standard way
/// to get a closed port and it is not perfectly race-free — something else could take it in the
/// window — which is why the test built on it accepts a refusal *or* a reset rather than exactly
/// one errno.
fn closed_port(family: IpFamily) -> SocketAddress {
    let (listener, address) = listening(family);
    drop(listener);
    address
}

/// A non-blocking connect over loopback settles, and `SO_ERROR` says it succeeded.
///
/// The two-step sequence is the whole point: `connect` answers *in progress*, the caller waits for
/// writability, and only then is `SO_ERROR` meaningful. A test that read `SO_ERROR` immediately
/// would read zero and pass against an implementation that never connected.
#[test]
fn a_nonblocking_connect_settles_and_reports_success_through_so_error() {
    let (listener, address) = listening(IpFamily::V4);
    let mut client = socket(SocketKind::Stream, IpFamily::V4);

    let progress = client.connect(&address).expect("connect over loopback");
    // Loopback can complete inside the call; anything off this machine cannot. Both are correct
    // answers and the test asserts the *outcome*, not which of them arrived.
    assert!(
        matches!(progress, ConnectProgress::InProgress | ConnectProgress::Connected),
        "{progress:?}"
    );

    wait_until(&client, Interest::WRITABLE, "the connect to settle", |r| r.writable || r.error);
    assert_eq!(
        client.connect_result().expect("the connect result"),
        ConnectOutcome::Connected,
        "a connect to a listening loopback port must succeed"
    );

    let accepted = listener.accept().expect("the listener accepts it").1;
    assert_eq!(
        SocketAddress::from_std(accepted),
        client.local_address().expect("our own address"),
        "the peer sees the address the connect bound us to"
    );
}

/// A connect to a port with nothing behind it fails, and the failure arrives through `SO_ERROR`.
///
/// **This is the test `WSAPoll` would have made impossible.** That call does not report a failed
/// connection attempt at all, so a backend built on it would leave this socket neither writable
/// nor in error and this test would time out. `select` reports it in `exceptfds`, which is why the
/// Windows backend uses `select` and says so.
#[test]
fn a_refused_connect_is_reported_rather_than_left_pending_for_ever() {
    let address = closed_port(IpFamily::V4);
    let mut client = socket(SocketKind::Stream, IpFamily::V4);

    let _ = client.connect(&address).expect("connect starts even when it will fail");
    wait_until(&client, Interest::WRITABLE, "the refused connect to settle", |r| {
        r.writable || r.error
    });

    let outcome = client.connect_result().expect("the connect result");
    let failed = match outcome {
        ConnectOutcome::Failed(
            kind @ (NetErrorKind::ConnectionRefused
            | NetErrorKind::ConnectionReset
            | NetErrorKind::TimedOut),
        ) => kind,
        other => panic!("a connect to a closed loopback port reported {other:?}"),
    };

    // `SO_ERROR` is consume-on-read on every target, and this seam neither hides that nor loses
    // the error to it: `connect_result` moved the value into the socket's pending slot, so the
    // guest's own `getsockopt(SO_ERROR)` still sees it — **once**.
    assert_eq!(
        client.get_option(SocketQuery::Error).expect("SO_ERROR"),
        OptionValue::Error(Some(failed)),
        "the error `connect_result` read must still be readable exactly once"
    );
    // **MEASURED, and it contradicts Winsock's own documentation.** `getsockopt(SO_ERROR)` is
    // documented — by POSIX and by Microsoft, whose page says "retrieve error status and clear" —
    // as consuming the error. On this host it does not, for a refused connect: this second read
    // goes to the stack (the pending slot was emptied above) and comes back with the same
    // `WSAECONNREFUSED`. The assertion is on what was measured rather than on what is documented,
    // and it is here so that a host which starts clearing is *noticed* instead of quietly
    // changing what the guest sees. The seam is correct either way: the pending slot exists so
    // that a host which does clear cannot lose the error to a host-side `connect_result`.
    assert_eq!(
        client.get_option(SocketQuery::Error).expect("SO_ERROR again"),
        OptionValue::Error(Some(failed)),
        "Winsock did not clear SO_ERROR on read for a refused connect"
    );
}

/// Bytes written on one side come out of the other, and a half-close is seen as end of file.
///
/// `Ok(0)` from `recv` is the *only* way end of file is reported on a stream, and a caller that
/// treats it as "nothing yet" loops for ever on a connection that is over — so the assertion is
/// on the zero, not on the absence of an error.
#[test]
fn bytes_cross_a_loopback_stream_and_a_half_close_arrives_as_end_of_file() {
    let (listener, address) = listening(IpFamily::V4);
    let mut client = socket(SocketKind::Stream, IpFamily::V4);
    let _ = client.connect(&address).expect("connect");
    wait_until(&client, Interest::WRITABLE, "the connect to settle", |r| r.writable || r.error);
    assert_eq!(client.connect_result().expect("result"), ConnectOutcome::Connected);
    let mut peer: HostStream = listener.accept().expect("accept").0;

    let sent = client.send(b"GET / HTTP/1.1\r\n").expect("a send on a connected stream");
    assert_eq!(sent, 16, "loopback takes the whole of a 16-byte write");
    let mut seen = [0_u8; 16];
    peer.read_exact(&mut seen).expect("the peer reads what was sent");
    assert_eq!(&seen, b"GET / HTTP/1.1\r\n");

    // Nothing has come back yet, and that is a would-block rather than an error.
    let mut buf = [0_u8; 64];
    let err = client.recv(&mut buf).unwrap_err();
    assert!(err.is_would_block(), "an empty non-blocking receive must say would-block: {err}");
    assert_eq!(err.kind(), Some(NetErrorKind::WouldBlock));

    peer.write_all(b"HTTP/1.1 200 OK").expect("the peer writes back");
    wait_until(&client, Interest::READABLE, "the reply", |r| r.readable);
    let read = client.recv(&mut buf).expect("the reply arrives");
    assert_eq!(&buf[..read], b"HTTP/1.1 200 OK");

    // The peer closes. End of file is a receive that returns zero, immediately and for ever.
    drop(peer);
    wait_until(&client, Interest::READABLE, "end of file", |r| r.readable);
    assert_eq!(client.recv(&mut buf).expect("end of file"), 0);
    assert_eq!(client.recv(&mut buf).expect("still end of file"), 0);
}

/// `shutdown(Write)` is seen by the peer as end of file while this side can still read.
#[test]
fn shutting_down_the_writing_half_is_end_of_file_for_the_peer_and_not_for_us() {
    let (listener, address) = listening(IpFamily::V4);
    let mut client = socket(SocketKind::Stream, IpFamily::V4);
    let _ = client.connect(&address).expect("connect");
    wait_until(&client, Interest::WRITABLE, "the connect to settle", |r| r.writable || r.error);
    assert_eq!(client.connect_result().expect("result"), ConnectOutcome::Connected);
    let mut peer: HostStream = listener.accept().expect("accept").0;

    client.shutdown(Shutdown::Write).expect("shutdown of the writing half");
    let mut drained = Vec::new();
    peer.read_to_end(&mut drained).expect("the peer reads to end of file");
    assert!(drained.is_empty(), "nothing was sent before the shutdown");

    // The reading half is still open, which is the whole difference between SHUT_WR and a close.
    peer.write_all(b"late").expect("the peer can still write to us");
    wait_until(&client, Interest::READABLE, "the late reply", |r| r.readable);
    let mut buf = [0_u8; 8];
    assert_eq!(client.recv(&mut buf).expect("a receive after SHUT_WR"), 4);
    assert_eq!(&buf[..4], b"late");
}

/// A datagram reaches a bound socket, and `recvfrom` says who sent it.
#[test]
fn a_datagram_arrives_at_a_bound_socket_with_the_senders_address() {
    let mut receiver = socket(SocketKind::Datagram, IpFamily::V4);

    // A fresh socket is unbound, exactly as `socket(2)` leaves it, and POSIX says `getsockname`
    // answers the wildcard address with port 0 for one.
    //
    // **MEASURED: Winsock does not.** `getsockname` on a socket nothing has named fails with
    // `WSAEINVAL` (10022) here, so the wildcard this assertion sees comes from the seam's own
    // knowledge that nothing has bound or connected the socket — see `Socket::local_address` for
    // why that is a fact rather than a substitute for one. The assertion is what a guest must be
    // able to rely on; the measurement is why there is code behind it.
    let before = receiver.local_address().expect("getsockname on an unbound socket");
    assert!(before.is_unspecified(), "{before}");
    assert_eq!(before.port(), 0, "an unbound socket has no port yet");

    receiver.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind to a loopback port");
    let bound = receiver.local_address().expect("getsockname after bind");
    assert!(bound.is_loopback(), "{bound}");
    assert_ne!(bound.port(), 0, "bind assigned an ephemeral port");

    let sender = HostDatagram::bind("127.0.0.1:0").expect("a host datagram socket");
    let sender_address = SocketAddress::from_std(sender.local_addr().expect("its address"));
    sender.send_to(b"ping", bound.to_std()).expect("send a datagram");

    wait_until(&receiver, Interest::READABLE, "the datagram", |r| r.readable);
    let mut buf = [0_u8; 32];
    let (read, from) = receiver.recv_from(&mut buf).expect("recvfrom");
    assert_eq!(&buf[..read], b"ping");
    assert_eq!(from, sender_address, "recvfrom must report who actually sent it");

    // And the reply goes back the other way, policy-checked at the call rather than at a connect
    // that never happened.
    let sent = receiver.send_to(b"pong", &sender_address).expect("sendto");
    assert_eq!(sent, 4);
    let mut back = [0_u8; 32];
    let (read, _) = sender.recv_from(&mut back).expect("the reply");
    assert_eq!(&back[..read], b"pong");
}

/// **A datagram too large to send is `MessageSize`, as `Socket::send_to` promises** -- not an
/// unclassified host error. 65,508 bytes is one more than an IPv4 UDP payload can be, so every host
/// refuses it; MEASURED first on the engine's path-MTU probe, which Windows refused with
/// `WSAEMSGSIZE` and `std` left uncategorised.
#[test]
fn an_oversized_datagram_is_message_size() {
    let sender = socket(SocketKind::Datagram, IpFamily::V4);
    let receiver = HostDatagram::bind("127.0.0.1:0").expect("a host datagram socket");
    let to = SocketAddress::from_std(receiver.local_addr().expect("its address"));
    let error = sender.send_to(&vec![0u8; 65_508], &to).expect_err("too large to send");
    assert_eq!(error.kind(), Some(NetErrorKind::MessageSize), "{error}");
}

/// An IPv6 loopback stream carries bytes, so the v6 marshalling is exercised and not only unit
/// tested.
///
/// `::1` exists on every Windows host with the IPv6 stack installed, which is the default and has
/// been since Vista. If it ever is not, this fails rather than skipping.
#[test]
fn an_ipv6_loopback_stream_connects_and_carries_bytes() {
    let (listener, address) = listening(IpFamily::V6);
    assert_eq!(address.family(), IpFamily::V6);
    let mut client = socket(SocketKind::Stream, IpFamily::V6);
    let _ = client.connect(&address).expect("a v6 connect over loopback");
    wait_until(&client, Interest::WRITABLE, "the v6 connect to settle", |r| r.writable || r.error);
    assert_eq!(client.connect_result().expect("result"), ConnectOutcome::Connected);

    let mut peer: HostStream = listener.accept().expect("accept").0;
    client.send(b"v6").expect("a send over v6 loopback");
    let mut seen = [0_u8; 2];
    peer.read_exact(&mut seen).expect("the peer reads it");
    assert_eq!(&seen, b"v6");
}

/// The options a client actually sets can be set and read back.
///
/// **The buffer assertions do not check equality**, and that is deliberate: every stack adjusts
/// `SO_RCVBUF` — Linux doubles it, Windows rounds — so a test that asserted the kernel returned
/// the number it was given would be asserting a behaviour no operating system has. What is
/// asserted is that the kernel agreed to *something* and that the something moved when the request
/// did.
#[test]
fn the_options_a_client_sets_take_effect_and_read_back() {
    let mut stream = socket(SocketKind::Stream, IpFamily::V4);

    stream.set_option(SocketOption::NoDelay(true)).expect("TCP_NODELAY on");
    assert_eq!(stream.get_option(SocketQuery::NoDelay).unwrap(), OptionValue::Flag(true));
    stream.set_option(SocketOption::NoDelay(false)).expect("TCP_NODELAY off");
    assert_eq!(stream.get_option(SocketQuery::NoDelay).unwrap(), OptionValue::Flag(false));

    stream.set_option(SocketOption::ReuseAddress(true)).expect("SO_REUSEADDR on");
    assert_eq!(stream.get_option(SocketQuery::ReuseAddress).unwrap(), OptionValue::Flag(true));

    let OptionValue::Bytes(default_receive) =
        stream.get_option(SocketQuery::ReceiveBuffer).unwrap()
    else {
        panic!("SO_RCVBUF must answer a size");
    };
    stream.set_option(SocketOption::ReceiveBuffer(default_receive * 4)).expect("SO_RCVBUF");
    let OptionValue::Bytes(raised) = stream.get_option(SocketQuery::ReceiveBuffer).unwrap() else {
        panic!("SO_RCVBUF must answer a size");
    };
    assert!(
        raised > default_receive,
        "asking for four times the default left it at {raised} (was {default_receive})"
    );

    let OptionValue::Bytes(default_send) = stream.get_option(SocketQuery::SendBuffer).unwrap()
    else {
        panic!("SO_SNDBUF must answer a size");
    };
    stream.set_option(SocketOption::SendBuffer(default_send * 4)).expect("SO_SNDBUF");
    let OptionValue::Bytes(raised_send) = stream.get_option(SocketQuery::SendBuffer).unwrap()
    else {
        panic!("SO_SNDBUF must answer a size");
    };
    assert!(raised_send > default_send, "{raised_send} vs {default_send}");

    let quarter = Duration::from_millis(250);
    stream.set_option(SocketOption::ReceiveTimeout(Some(quarter))).expect("SO_RCVTIMEO");
    assert_eq!(
        stream.get_option(SocketQuery::ReceiveTimeout).unwrap(),
        OptionValue::Timeout(Some(quarter))
    );
    stream.set_option(SocketOption::SendTimeout(Some(quarter))).expect("SO_SNDTIMEO");
    assert_eq!(
        stream.get_option(SocketQuery::SendTimeout).unwrap(),
        OptionValue::Timeout(Some(quarter))
    );
    // A zero timeval clears the timeout on a device, and `std` spells that `None`. A seam that
    // passed the zero straight through would report EINVAL for a legal call.
    stream.set_option(SocketOption::ReceiveTimeout(Some(Duration::ZERO))).expect("clearing it");
    assert_eq!(
        stream.get_option(SocketQuery::ReceiveTimeout).unwrap(),
        OptionValue::Timeout(None)
    );

    let mut six = socket(SocketKind::Stream, IpFamily::V6);
    six.set_option(SocketOption::V6Only(true)).expect("IPV6_V6ONLY on");
    assert_eq!(six.get_option(SocketQuery::V6Only).unwrap(), OptionValue::Flag(true));
    six.set_option(SocketOption::V6Only(false)).expect("IPV6_V6ONLY off");
    assert_eq!(six.get_option(SocketQuery::V6Only).unwrap(), OptionValue::Flag(false));
}

/// An option that is not defined on this socket is refused by name rather than accepted.
///
/// Rule 1's shape: a caller told its `setsockopt` succeeded believes the option took effect. Each
/// of these would be `ENOPROTOOPT` on a device.
#[test]
fn an_option_that_does_not_apply_to_this_socket_is_refused_and_names_itself() {
    let mut datagram = socket(SocketKind::Datagram, IpFamily::V4);
    let err = datagram.set_option(SocketOption::NoDelay(true)).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("TCP_NODELAY"), "{text}");
    assert!(text.contains("ENOPROTOOPT"), "{text}");
    assert!(matches!(err, NetError::Refused { .. }), "{err}");

    let mut four = socket(SocketKind::Stream, IpFamily::V4);
    let err = four.set_option(SocketOption::V6Only(true)).unwrap_err();
    assert!(err.to_string().contains("IPV6_V6ONLY"), "{err}");
    let err = four.get_option(SocketQuery::V6Only).unwrap_err();
    assert!(err.to_string().contains("IPv4"), "{err}");
}

/// An operation that is not defined on this socket kind is refused by name, not quietly reshaped.
#[test]
fn an_operation_that_does_not_apply_to_this_socket_kind_is_refused_by_name() {
    let stream = socket(SocketKind::Stream, IpFamily::V4);
    let destination = SocketAddress::loopback(IpFamily::V4, 9);
    let err = stream.send_to(b"x", &destination).unwrap_err();
    assert!(err.to_string().contains("EISCONN"), "{err}");
    assert!(
        err.to_string().contains("refused rather than ignored"),
        "the message must say the address was not silently dropped: {err}"
    );
    let mut buf = [0_u8; 4];
    let err = stream.recv_from(&mut buf).unwrap_err();
    assert!(err.to_string().contains("no source address"), "{err}");

    let datagram = socket(SocketKind::Datagram, IpFamily::V4);
    let err = datagram.shutdown(Shutdown::Both).unwrap_err();
    assert!(err.to_string().contains("shutdown(2)"), "the refusal names the missing work: {err}");
}

/// An address of the wrong family is refused before it reaches the host.
#[test]
fn an_address_of_the_wrong_family_is_refused_rather_than_converted() {
    let mut four = socket(SocketKind::Stream, IpFamily::V4);
    let six = SocketAddress::loopback(IpFamily::V6, 443);
    let err = four.connect(&six).unwrap_err();
    assert_eq!(err.kind(), Some(NetErrorKind::AddressFamilyNotSupported));
    assert!(err.to_string().contains("IPV6_V6ONLY"), "the message says why it is not converted: {err}");
}

/// The policy refuses a destination before any packet leaves, on a socket that could have reached
/// it.
///
/// The socket here is real and the host would have connected; what stops it is the embedding's
/// rule. That is D30's whole shape asserted against a working stack rather than against a type.
#[test]
fn the_policy_stops_a_real_socket_before_the_host_is_asked() {
    let mut client = Socket::new(
        SocketKind::Stream,
        IpFamily::V4,
        Arc::new(NetPolicy::closed().allow_host_suffix("roblox.com").allow_port(443)),
    )
    .expect("a socket");
    client.set_nonblocking(true).expect("non-blocking");

    // Loopback is not in this policy, and neither is the port.
    let err = client.connect(&SocketAddress::loopback(IpFamily::V4, 8_080)).unwrap_err();
    assert!(matches!(err, NetError::Policy { .. }), "{err}");
    assert!(err.to_string().contains("does not allow loopback"), "{err}");
    assert_eq!(client.connect_result().expect("no connect was started"), ConnectOutcome::NotStarted);
}

/// `poll` reports the socket with data and not the one without.
///
/// The negative half is the assertion that matters: a readiness call that answered "ready" for
/// everything would pass a test that only checked the socket it had just written to.
#[test]
fn poll_names_the_socket_that_has_something_and_not_the_one_that_does_not() {
    let mut busy = socket(SocketKind::Datagram, IpFamily::V4);
    busy.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind");
    let busy_address = busy.local_address().expect("its address");

    let mut idle = socket(SocketKind::Datagram, IpFamily::V4);
    idle.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind");

    let sender = HostDatagram::bind("127.0.0.1:0").expect("a host datagram socket");
    sender.send_to(b"wake", busy_address.to_std()).expect("send");

    let deadline = Instant::now() + DEADLINE;
    loop {
        let mut entries = [
            PollEntry::new(&busy, Interest::READABLE),
            PollEntry::new(&idle, Interest::READABLE),
        ];
        let ready = poll(&mut entries, Duration::from_millis(50)).expect("poll");
        if entries[0].readiness().readable {
            assert_eq!(ready, 1, "exactly one socket has something to read");
            assert!(
                !entries[1].readiness().readable,
                "a socket nobody sent anything to must not be reported readable"
            );
            break;
        }
        assert!(Instant::now() < deadline, "the datagram never arrived");
    }

    // An unbound, idle datagram socket is writable and not readable, which is what `Entry::Socket`
    // will answer in the descriptor table.
    let readiness = idle.readiness().expect("readiness of one socket");
    assert!(readiness.writable, "an idle datagram socket can be sent from");
    assert!(!readiness.readable, "and has nothing to read");
    assert!(!readiness.error, "and nothing is wrong with it");
}

/// A set larger than the backend can express is refused by name, not truncated.
///
/// A truncated poll answers "not ready" about sockets it never looked at, and the caller cannot
/// tell that from a timeout. The refusal names the limit, the reason, and what would lift it.
#[test]
fn a_poll_set_past_the_backends_limit_is_refused_rather_than_silently_shortened() {
    let sockets: Vec<Socket> = (0..=MAX_POLL_SOCKETS)
        .map(|_| socket(SocketKind::Datagram, IpFamily::V4))
        .collect();
    let mut entries: Vec<PollEntry<'_>> =
        sockets.iter().map(|s| PollEntry::new(s, Interest::READABLE)).collect();
    assert_eq!(entries.len(), MAX_POLL_SOCKETS + 1);

    let err = poll(&mut entries, Duration::ZERO).unwrap_err();
    let text = err.to_string();
    assert!(text.contains(&MAX_POLL_SOCKETS.to_string()), "{text}");
    assert!(text.contains("FD_SETSIZE"), "{text}");
    assert!(text.contains("WSAPoll"), "the refusal names what would lift it: {text}");

    // One fewer is served, so the limit is where it says it is rather than one off.
    let exactly = &mut entries[..MAX_POLL_SOCKETS];
    assert_eq!(poll(exactly, Duration::ZERO).expect("the limit itself is allowed"), 0);
}

/// A socket nothing has done anything to still answers a readiness question.
///
/// **This is the state `Entry::Socket` will be in most often** — a descriptor the guest has
/// created and not yet used — and a `poll` over a mixed descriptor set has to survive it. It is
/// asserted separately because every other test here polls a socket that is bound or connected,
/// and "works once something has happened to it" is a weaker claim than the descriptor table
/// needs.
#[test]
fn a_fresh_socket_answers_a_readiness_question_rather_than_refusing_one() {
    for kind in [SocketKind::Stream, SocketKind::Datagram] {
        for family in [IpFamily::V4, IpFamily::V6] {
            let fresh = socket(kind, family);
            let readiness = fresh
                .readiness()
                .unwrap_or_else(|e| panic!("readiness of a fresh {family} {kind:?} socket: {e}"));
            assert!(
                !readiness.readable,
                "a {family} {kind:?} socket nothing has sent to must not be readable"
            );
            assert!(!readiness.error, "and nothing is wrong with it: {readiness:?}");
        }
    }
}

/// **The three keep-alive timing options land on the three host options they name**, proved by
/// giving each a distinct value and reading all three back.
///
/// This is the detector for the defect the whole `KeepAliveIdle` doc comment is about, and the
/// shape of the test is the whole point. Linux numbers these 4, 5, 6 and Windows numbers them 3,
/// 17, 16, with `TCP_MAXRT` sitting on 5 — so **every wrong mapping succeeds**. `setsockopt`
/// returns zero, the socket carries on working, and the only observable difference is a
/// keep-alive that fires at the wrong time, minutes later, on a connection nobody is watching.
///
/// Three distinct values are what separates a correct mapping from a transposed one. If the test
/// set all three to the same number, a backend that wrote the idle time into `TCP_KEEPINTVL` and
/// back would pass; if it read only one of them back, a swap of the other two would pass. This
/// sets 120, 31 and 7 — deliberately unequal, and none of them a default on either host — and
/// asserts all three.
#[test]
fn the_keep_alive_timing_options_round_trip_to_three_distinct_values() {
    let (listener, address) = listening(IpFamily::V4);
    let mut client = socket(SocketKind::Stream, IpFamily::V4);
    let _ = client.connect(&address).expect("connect");
    wait_until(&client, Interest::WRITABLE, "the connect to settle", |r| r.writable || r.error);
    assert_eq!(client.connect_result().expect("result"), ConnectOutcome::Connected);
    let _peer = listener.accept().expect("accept").0;

    // The switch first, because the timing options configure something that is off by default.
    client.set_option(SocketOption::KeepAlive(true)).expect("SO_KEEPALIVE");
    client
        .set_option(SocketOption::KeepAliveIdle(Duration::from_secs(120)))
        .expect("the keep-alive idle time");
    client
        .set_option(SocketOption::KeepAliveInterval(Duration::from_secs(31)))
        .expect("the keep-alive probe interval");
    client.set_option(SocketOption::KeepAliveCount(7)).expect("the keep-alive probe count");

    assert_eq!(
        client.get_option(SocketQuery::KeepAliveIdle).expect("read the idle time"),
        OptionValue::Interval(Duration::from_secs(120)),
        "the idle time must come back from the option it was written to"
    );
    assert_eq!(
        client.get_option(SocketQuery::KeepAliveInterval).expect("read the interval"),
        OptionValue::Interval(Duration::from_secs(31)),
        "31 is the interval; if this reads 120 the two options are transposed"
    );
    assert_eq!(
        client.get_option(SocketQuery::KeepAliveCount).expect("read the count"),
        OptionValue::Count(7)
    );
    assert_eq!(
        client.get_option(SocketQuery::KeepAlive).expect("read the switch"),
        OptionValue::Flag(true),
        "and the timing must not have disturbed the switch"
    );
}

/// **A fractional interval is refused rather than rounded**, and so is one past the host's field.
///
/// Neither is reachable from the guest adapter, which carries an `int` of seconds — which is
/// exactly why it is asserted here, from the crate's own API, rather than assumed to be
/// unreachable. Rounding 500 ms to a second would set an interval twice what was asked for and
/// report success; rounding it to zero would configure a socket to probe with no idle time at
/// all.
#[test]
fn a_fractional_keep_alive_interval_is_refused_rather_than_rounded() {
    let mut client = socket(SocketKind::Stream, IpFamily::V4);
    for bad in [Duration::from_millis(500), Duration::new(120, 1)] {
        let error = client
            .set_option(SocketOption::KeepAliveIdle(bad))
            .expect_err("a fraction of a second cannot be set on either host");
        assert_eq!(error.kind(), Some(NetErrorKind::InvalidInput), "{error}");
        let text = error.to_string();
        assert!(text.contains("whole seconds"), "the refusal must say why: {text}");
    }
    let error = client
        .set_option(SocketOption::KeepAliveInterval(Duration::from_secs(
            u64::from(u32::MAX) + 1,
        )))
        .expect_err("past the 32-bit field both hosts carry it in");
    assert_eq!(error.kind(), Some(NetErrorKind::InvalidInput), "{error}");
}

/// **The keep-alive timing options are TCP options and are refused by name on a datagram socket.**
///
/// A device answers `ENOPROTOOPT`. Accepting them would be the plausible stub in its purest form:
/// a UDP socket has no keep-alive, so nothing would ever observe that the call did nothing.
#[test]
fn the_keep_alive_timing_options_are_refused_on_a_datagram_socket() {
    let mut datagram = socket(SocketKind::Datagram, IpFamily::V4);
    for option in [
        SocketOption::KeepAliveIdle(Duration::from_secs(60)),
        SocketOption::KeepAliveInterval(Duration::from_secs(10)),
        SocketOption::KeepAliveCount(3),
    ] {
        let error = datagram
            .set_option(option)
            .expect_err("a TCP option on a datagram socket must be refused");
        assert!(
            matches!(error, NetError::Refused { .. }),
            "{option:?} must be refused by name, and it produced {error:?}"
        );
        let text = error.to_string();
        assert!(text.contains("keep-alive"), "the refusal names the option: {text}");
        assert!(text.contains("SOCK_DGRAM"), "and what it was asked of: {text}");
    }
}

/// A socket closes when it is dropped, which is why there is no `close`.
#[test]
fn dropping_a_socket_closes_it_and_the_peer_sees_the_close() {
    let (listener, address) = listening(IpFamily::V4);
    let mut client = socket(SocketKind::Stream, IpFamily::V4);
    let _ = client.connect(&address).expect("connect");
    wait_until(&client, Interest::WRITABLE, "the connect to settle", |r| r.writable || r.error);
    assert_eq!(client.connect_result().expect("result"), ConnectOutcome::Connected);
    let mut peer: HostStream = listener.accept().expect("accept").0;

    drop(client);

    let mut drained = Vec::new();
    peer.read_to_end(&mut drained).expect("the peer reads to end of file after we dropped");
    assert!(drained.is_empty());
}
