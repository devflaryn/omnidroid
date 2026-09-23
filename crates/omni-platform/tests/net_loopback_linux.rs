//! Real sockets over loopback on **Linux**: the Linux mirror of `net_loopback.rs`, which is
//! compiled only on Windows.
//!
//! ```text
//! cargo test -p omni-platform --release --test net_loopback_linux
//! ```
//!
//! Every test of the Windows file is here, against this host's backend, with the assertions that
//! were **measurements of Winsock** replaced by what this kernel does -- each one named where it
//! differs, because a mirror that quietly copied a Windows fact would be asserting the wrong host:
//!
//! | measured on Windows | on Linux (MEASURED here) |
//! |---|---|
//! | `SO_ERROR` is **not** cleared by a read, for a refused connect | it **is** cleared: the second read is `None` |
//! | `getsockname` on an unbound socket fails `WSAEINVAL` | the kernel answers `0.0.0.0:0` itself |
//! | `hangup` is never set (`select` has no such signal) | `POLLHUP`: a fresh TCP socket, a reset, a close in both directions |
//! | `SO_RCVBUF` reads back rounded | reads back **doubled**, and clamped at `2 * net.core.rmem_max` |
//! | `SO_LINGER` refused on UDP, `SO_BROADCAST` on TCP | both accepted on both (the seam keeps them anyway) |
//!
//! Plus what only this backend can show: the kernel's own `EALREADY` for a second connect, the
//! keep-alive ranges the kernel enforces, and a hang-up `poll(2)` reports unasked.
//!
//! The policy is [`NetPolicy::loopback_only`] throughout, so nothing here can leave the machine
//! even if a test were written wrongly: the policy refuses the address before any packet leaves.
//! Nothing here depends on any Roblox endpoint, or on DNS.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream as HostStream, UdpSocket as HostDatagram};
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_platform::net::{
    poll, ConnectOutcome, ConnectProgress, Interest, IpFamily, NetError, NetErrorKind, NetPolicy,
    OptionValue, PathMtu, PollEntry, Shutdown, Socket, SocketAddress, SocketKind, SocketOption,
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
/// On Windows this is the test `WSAPoll` would have made impossible. Here `poll(2)` reports the
/// failure itself -- `POLLERR` (with `POLLHUP` and `POLLOUT`) -- and `SO_ERROR` holds
/// `ECONNREFUSED`; the extra assertion below is the `error` bit a poll that dropped `POLLERR`
/// would lose.
#[test]
fn a_refused_connect_is_reported_rather_than_left_pending_for_ever() {
    let address = closed_port(IpFamily::V4);
    let mut client = socket(SocketKind::Stream, IpFamily::V4);

    let _ = client.connect(&address).expect("connect starts even when it will fail");
    wait_until(&client, Interest::WRITABLE, "the refused connect to settle", |r| {
        r.writable || r.error
    });

    let settled = client.readiness().expect("readiness after the refusal");
    assert!(settled.error && settled.hangup, "a refused connect is POLLERR|POLLHUP: {settled:?}");
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
    // **MEASURED: Linux clears `SO_ERROR` on read, as POSIX documents** -- the opposite of what
    // Winsock was measured doing. This second read goes to the kernel (the pending slot was
    // emptied above), and the kernel's `sock_error` has already exchanged it for zero. This is
    // the host the pending slot exists for: without it, `connect_result`'s read would have been
    // the only one, and the guest's own `getsockopt(SO_ERROR)` would have found nothing.
    assert_eq!(
        client.get_option(SocketQuery::Error).expect("SO_ERROR again"),
        OptionValue::Error(None),
        "Linux clears SO_ERROR when it is read"
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
    // answers the wildcard address with port 0 for one. **Linux does**, itself -- the seam's
    // fallback for Winsock's `WSAEINVAL` is never reached on this host.
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
/// `::1` exists on this host (`ip -br addr`: `lo ... ::1/128`). A kernel booted with
/// `ipv6.disable=1` would not have it, and this then fails rather than skipping.
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
/// **The buffer assertions here do not check equality** -- the mirror of the Windows test, which
/// asserts only that the kernel agreed to *something* that moved. Linux's exact rule is asserted
/// separately, in `the_kernel_doubles_a_buffer_size_and_the_seam_reports_the_doubled_figure`.
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

/// **`SO_LINGER` and `SO_BROADCAST` are accepted on every socket kind, as Linux accepts them**:
/// the host's on the kind it serves each on, kept by the seam on the other (where Winsock refuses
/// them; this host would not, and the seam keeps them anyway so both hosts answer alike).
/// MEASURED why: the engine's game-socket setup sets `SO_LINGER` off and `SO_BROADCAST` on on its
/// UDP socket, and died on the first (2026-09-23).
#[test]
fn linger_and_broadcast_are_accepted_on_every_socket_kind_as_linux_accepts_them() {
    for family in [IpFamily::V4, IpFamily::V6] {
        // Stream: SO_LINGER is the host's -- off, or on with a zero time.
        let mut stream = socket(SocketKind::Stream, family);
        let linger = |s: &mut Socket| s.get_option(SocketQuery::Linger).expect("SO_LINGER");
        assert_eq!(linger(&mut stream), OptionValue::Linger(None), "off by default");
        stream.set_option(SocketOption::Linger(Some(Duration::ZERO))).expect("the abortive close");
        assert_eq!(linger(&mut stream), OptionValue::Linger(Some(Duration::ZERO)));
        stream.set_option(SocketOption::Linger(None)).expect("off again");
        assert_eq!(linger(&mut stream), OptionValue::Linger(None));
        let err = stream.set_option(SocketOption::Linger(Some(Duration::from_secs(5)))).unwrap_err();
        assert!(matches!(err, NetError::Refused { .. }), "{err}");
        assert!(err.to_string().contains("SO_LINGER"), "{err}");
        assert_eq!(linger(&mut stream), OptionValue::Linger(None), "a refused set changes nothing");

        // Stream: SO_BROADCAST is kept.
        let broadcast = |s: &mut Socket| s.get_option(SocketQuery::Broadcast).expect("SO_BROADCAST");
        assert_eq!(broadcast(&mut stream), OptionValue::Flag(false));
        stream.set_option(SocketOption::Broadcast(true)).expect("kept, as Linux keeps it");
        assert_eq!(broadcast(&mut stream), OptionValue::Flag(true));
        stream.set_option(SocketOption::Broadcast(false)).expect("and cleared");
        assert_eq!(broadcast(&mut stream), OptionValue::Flag(false));

        // Datagram: SO_BROADCAST is the host's, SO_LINGER is kept -- any time, as Linux keeps it.
        let mut datagram = socket(SocketKind::Datagram, family);
        assert_eq!(broadcast(&mut datagram), OptionValue::Flag(false));
        datagram.set_option(SocketOption::Broadcast(true)).expect("SO_BROADCAST on");
        assert_eq!(broadcast(&mut datagram), OptionValue::Flag(true));
        datagram.set_option(SocketOption::Broadcast(false)).expect("SO_BROADCAST off");
        assert_eq!(broadcast(&mut datagram), OptionValue::Flag(false));
        assert_eq!(linger(&mut datagram), OptionValue::Linger(None));
        datagram.set_option(SocketOption::Linger(None)).expect("off, the engine's call");
        datagram.set_option(SocketOption::Linger(Some(Duration::from_secs(5)))).expect("kept");
        assert_eq!(linger(&mut datagram), OptionValue::Linger(Some(Duration::from_secs(5))));
        datagram.set_option(SocketOption::Linger(None)).expect("off again");
        assert_eq!(linger(&mut datagram), OptionValue::Linger(None));
    }
}

/// **The host's `SO_LINGER` takes effect**: with it on and a zero time, closing a connected stream
/// resets the peer, where the default close ends the peer's stream. A seam that kept the option
/// instead of setting it would read back the same and fail only here.
#[test]
fn an_abortive_linger_resets_the_peer_where_the_default_close_ends_its_stream() {
    for abortive in [false, true] {
        let (listener, address) = listening(IpFamily::V4);
        let mut client = socket(SocketKind::Stream, IpFamily::V4);
        let _ = client.connect(&address).expect("connect");
        wait_until(&client, Interest::WRITABLE, "the connect to settle", |r| r.writable || r.error);
        assert_eq!(client.connect_result().expect("result"), ConnectOutcome::Connected);
        let mut peer: HostStream = listener.accept().expect("accept").0;
        peer.set_read_timeout(Some(DEADLINE)).expect("a bounded read");
        if abortive {
            client.set_option(SocketOption::Linger(Some(Duration::ZERO))).expect("SO_LINGER {1, 0}");
        }
        drop(client);
        let mut buf = [0_u8; 8];
        let read = peer.read(&mut buf);
        if abortive {
            let err = read.expect_err("an abortive close is a reset, not end of file");
            assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset, "{err}");
        } else {
            assert_eq!(read.expect("the default close"), 0, "end of file");
        }
    }
}

/// **Path-MTU discovery takes `DONT`, `DO` and `PROBE` in any order on one socket, and the host
/// reads each back**, both families. MEASURED why: RakNet's join sets `PROBE` for its MTU probes
/// and then `DONT` on the same socket (2026-09-23). Under the old mapping onto Windows'
/// `IP_DONTFRAGMENT` `PROBE` had no spelling, and Windows refuses (`WSAEINVAL`, MEASURED) to mix
/// `IP_DONTFRAGMENT` and `IP_MTU_DISCOVER` on one socket -- so every mode has to be the latter.
#[test]
fn path_mtu_discovery_takes_every_mode_in_any_order_on_one_socket() {
    for family in [IpFamily::V4, IpFamily::V6] {
        let mut datagram = socket(SocketKind::Datagram, family);
        let mode = |s: &mut Socket| s.get_option(SocketQuery::PathMtuDiscovery).expect("read back");
        assert_eq!(mode(&mut datagram), OptionValue::PathMtu(None), "{family}: the host's default");
        for want in [PathMtu::Probe, PathMtu::Dont, PathMtu::Do, PathMtu::Probe, PathMtu::Dont] {
            datagram
                .set_option(SocketOption::PathMtuDiscovery(want))
                .unwrap_or_else(|e| panic!("{family} {want:?}: {e}"));
            assert_eq!(mode(&mut datagram), OptionValue::PathMtu(Some(want)), "{family}");
        }
    }
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

/// **A socket deep in a set is looked at**: a hundred bound sockets, one datagram to the
/// ninety-first, and exactly that one is readable -- the Windows file's test for its wide
/// `fd_set`, which here is the check that `revents` is read back into the right entry.
#[test]
fn a_socket_beyond_the_default_fd_setsize_is_seen() {
    const SOCKETS: usize = 100;
    const WOKEN: usize = 90;
    let sockets: Vec<Socket> = (0..SOCKETS)
        .map(|_| {
            let mut socket = socket(SocketKind::Datagram, IpFamily::V4);
            socket.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind");
            socket
        })
        .collect();
    let target = sockets[WOKEN].local_address().expect("its address");
    let sender = HostDatagram::bind("127.0.0.1:0").expect("a host datagram socket");
    sender.send_to(b"wake", target.to_std()).expect("send");

    let deadline = Instant::now() + DEADLINE;
    loop {
        let mut entries: Vec<PollEntry<'_>> =
            sockets.iter().map(|s| PollEntry::new(s, Interest::READABLE)).collect();
        let ready = poll(&mut entries, Duration::from_millis(50)).expect("poll");
        let readable: Vec<usize> =
            (0..SOCKETS).filter(|&i| entries[i].readiness().readable).collect();
        if !readable.is_empty() {
            assert_eq!(readable, vec![WOKEN], "only the socket that was sent to");
            assert_eq!(ready, 1);
            break;
        }
        assert!(Instant::now() < deadline, "the datagram to socket {WOKEN} was never seen");
    }
}

/// A set larger than the backend can express is refused by name, not truncated.
///
/// A truncated poll answers "not ready" about sockets it never looked at, and the caller cannot
/// tell that from a timeout. The refusal names the limit, the reason, and what would lift it.
#[test]
fn a_poll_set_past_the_backends_limit_is_refused_rather_than_silently_shortened() {
    // 1,025 sockets is past a desktop session's default soft `RLIMIT_NOFILE` of 1,024; the hard
    // limit is far higher, and the soft one is this process's to raise.
    raise_descriptor_limit(4096);
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
    // The refusal is the seam's, made before any backend is asked, so it is the same text on
    // every host -- including this one, whose `poll(2)` has no such limit (see `net::unix`).

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
            // **MEASURED, Linux's `tcp_poll`/`udp_poll`**: a TCP socket nothing has connected is
            // in `TCP_CLOSE`, which Linux reports as `POLLOUT | POLLHUP`; a UDP socket is only
            // writable. `poll(2)` sets `POLLHUP` unasked, and the seam passes it on.
            assert!(readiness.writable, "{family} {kind:?}: {readiness:?}");
            assert_eq!(
                readiness.hangup,
                kind == SocketKind::Stream,
                "{family} {kind:?}: only an unconnected TCP socket reports hang-up: {readiness:?}"
            );
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

// ================================================================== listen, accept

/// A stream socket listening on loopback, bound to an ephemeral port.
fn listener_on(family: IpFamily, policy: Arc<NetPolicy>) -> (Socket, SocketAddress) {
    let mut listener = Socket::new(SocketKind::Stream, family, policy).expect("a stream socket");
    listener.set_nonblocking(true).expect("non-blocking mode");
    listener.bind(&SocketAddress::loopback(family, 0)).expect("bind loopback:0");
    listener.listen(8).expect("listen on loopback under a loopback policy");
    let local = listener.local_address().expect("the bound address");
    assert_ne!(local.port(), 0, "listen on an ephemeral port gives it a number");
    (listener, local)
}

/// **A connection is accepted, carries data both ways, and the accepted socket is BLOCKING
/// although the listener is not** -- Linux's accept(2) does not pass `O_NONBLOCK` on, where
/// Winsock's accepted socket inherits it. MEASURED why listen/accept exist at all: the engine's
/// MicroProfiler web server bound `0.0.0.0:1338` and died on an unbound `listen` (2026-09-23).
#[test]
fn a_listener_accepts_a_connection_and_the_accepted_socket_is_blocking() {
    for family in [IpFamily::V4, IpFamily::V6] {
        let (listener, local) = listener_on(family, loopback_policy());
        match listener.accept() {
            Err(error) => assert_eq!(error.kind(), Some(NetErrorKind::WouldBlock), "{error}"),
            Ok(_) => panic!("{family}: accepted with nobody connecting"),
        }
        let mut client = HostStream::connect(local.to_std()).expect("connect to the listener");
        wait_until(&listener, Interest::READABLE, "a pending connection", |r| r.readable);
        let (accepted, peer) = listener.accept().expect("the pending connection");
        assert_eq!(peer.to_std(), client.local_addr().expect("the client's end"), "{family}");
        assert_eq!(accepted.peer_address().expect("peer").to_std(), peer.to_std());
        assert_eq!(accepted.family(), family);
        assert_eq!(accepted.kind(), SocketKind::Stream);
        assert!(!accepted.nonblocking(), "{family}: the accepted socket reports blocking");

        // Blocking in fact, not just in its flag: with a 150 ms receive timeout and nothing sent,
        // a blocking recv waits the timeout out, where a non-blocking one returns at once.
        let mut accepted = accepted;
        accepted
            .set_option(SocketOption::ReceiveTimeout(Some(Duration::from_millis(150))))
            .expect("SO_RCVTIMEO");
        let started = Instant::now();
        let mut buf = [0u8; 16];
        let empty = accepted.recv(&mut buf);
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "{family}: recv returned after {:?} ({empty:?}); the accepted socket is non-blocking",
            started.elapsed()
        );
        accepted.set_option(SocketOption::ReceiveTimeout(None)).expect("no timeout");

        client.write_all(b"hello").expect("the client sends");
        let mut got = [0u8; 5];
        let mut read = 0;
        while read < 5 {
            read += accepted.recv(&mut got[read..]).expect("the accepted socket receives");
        }
        assert_eq!(&got, b"hello");
        assert_eq!(accepted.send(b"world").expect("the accepted socket sends"), 5);
        let mut back = [0u8; 5];
        client.read_exact(&mut back).expect("the client receives");
        assert_eq!(&back, b"world");
    }
}

/// **`listen` asks the policy about the address the socket is bound to**, and refuses by policy
/// before the host is asked; an unbound socket is judged as the wildcard address `listen` would
/// bind it to.
#[test]
fn listen_is_asked_of_the_policy_with_the_bound_address() {
    let is_policy = |result: Result<(), NetError>| matches!(result, Err(NetError::Policy { .. }));
    // Loopback-only: a wildcard bind may not listen, and neither may an unbound socket.
    let mut wildcard = Socket::new(SocketKind::Stream, IpFamily::V4, loopback_policy()).expect("socket");
    wildcard.bind(&SocketAddress::unspecified(IpFamily::V4)).expect("bind 0.0.0.0:0");
    assert!(is_policy(wildcard.listen(8)), "a wildcard listener under a loopback-only policy");
    let mut unbound = Socket::new(SocketKind::Stream, IpFamily::V4, loopback_policy()).expect("socket");
    assert!(is_policy(unbound.listen(8)), "an unbound socket is judged as the wildcard");
    // A closed policy refuses even loopback, and `allow_listen` alone admits it.
    let closed = Arc::new(NetPolicy::closed());
    let mut refused = Socket::new(SocketKind::Stream, IpFamily::V4, closed).expect("socket");
    refused.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind loopback:0");
    let refusal = refused.listen(8).expect_err("a closed policy refuses listening");
    assert!(matches!(refusal, NetError::Policy { .. }), "{refusal:?}");
    assert!(refusal.to_string().contains("127.0.0.1"), "{refusal}");
    let (_listener, local) = listener_on(IpFamily::V4, Arc::new(NetPolicy::closed().allow_listen()));
    assert!(local.is_loopback());
}

/// `accept` on a stream socket that is not listening is Linux's `EINVAL`, and both calls on a
/// datagram socket are refused for the adapter to answer `EOPNOTSUPP`.
#[test]
fn accept_needs_a_listener_and_neither_call_takes_a_datagram_socket() {
    let mut idle = Socket::new(SocketKind::Stream, IpFamily::V4, loopback_policy()).expect("socket");
    idle.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind loopback:0");
    let not_listening = idle.accept().expect_err("accept without listen");
    assert_eq!(not_listening.kind(), Some(NetErrorKind::InvalidInput), "{not_listening}");
    let mut datagram = socket(SocketKind::Datagram, IpFamily::V4);
    datagram.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind loopback:0");
    assert!(matches!(datagram.listen(8), Err(NetError::Refused { .. })));
    assert!(matches!(datagram.accept(), Err(NetError::Refused { .. })));
}

/// **The host's interface addresses include loopback and the address a route would use** -- the
/// second is an independent oracle: a UDP `connect` sends nothing and makes the host choose the
/// local address it would route from (TEST-NET-1, RFC 5737, reaches nobody). MEASURED reader: the
/// Java side's `NetworkUtils.getPublicIPv4Addresseses`, whose refusal froze the game (2026-09-23).
#[test]
fn the_hosts_interface_addresses_include_loopback_and_the_routed_address() {
    let addresses = omni_platform::net::interface_addresses().expect("the host's adapters");
    let loopback: std::net::IpAddr = "127.0.0.1".parse().expect("literal");
    assert!(addresses.contains(&loopback), "{addresses:?}");
    // Both families: `lo` carries `::1` on this host (`ip -br addr`), and getifaddrs lists it.
    let loopback6: std::net::IpAddr = "::1".parse().expect("literal");
    assert!(addresses.contains(&loopback6), "{addresses:?}");
    let probe = HostDatagram::bind("0.0.0.0:0").expect("a UDP socket");
    probe.connect("192.0.2.1:9").expect("a route to TEST-NET-1 (this host has a network)");
    let routed = probe.local_addr().expect("the routed local address").ip();
    assert!(addresses.contains(&routed), "{routed} is not among {addresses:?}");
    let distinct: std::collections::BTreeSet<_> = addresses.iter().collect();
    assert_eq!(distinct.len(), addresses.len(), "no address is listed twice: {addresses:?}");
}

// ================================================================== Linux only

/// Raise this process's soft descriptor limit to at least `want`, within the hard limit.
fn raise_descriptor_limit(want: u64) {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `limit` is a live `rlimit` the call writes.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) }, 0);
    if limit.rlim_cur < want {
        limit.rlim_cur = want.min(limit.rlim_max);
        // SAFETY: as above; raising the soft limit within the hard one needs no privilege.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) }, 0);
    }
}

/// `/proc/sys/net/core/<name>` as a number.
fn sysctl(name: &str) -> usize {
    std::fs::read_to_string(format!("/proc/sys/net/core/{name}"))
        .unwrap_or_else(|e| panic!("net.core.{name}: {e}"))
        .trim()
        .parse()
        .expect("a number")
}

/// **Linux doubles a buffer size, and the seam reports the kernel's figure** -- not the request.
///
/// `socket(7)`: "the kernel doubles this value (to allow space for bookkeeping overhead) when it
/// is set ... and this doubled value is returned by getsockopt", after clamping the request to
/// `net.core.rmem_max`/`wmem_max`. So 65,536 reads back as 131,072, and a request past the
/// maximum reads back as twice the maximum. A seam that halved the answer "to match what was
/// asked" would read 65,536 here and fail; one that reported the request would fail the clamp.
#[test]
fn the_kernel_doubles_a_buffer_size_and_the_seam_reports_the_doubled_figure() {
    for kind in [SocketKind::Stream, SocketKind::Datagram] {
        let mut s = socket(kind, IpFamily::V4);
        let bytes = |s: &mut Socket, q| match s.get_option(q).expect("read back") {
            OptionValue::Bytes(n) => n,
            other => panic!("{other:?}"),
        };
        for (set, query, max) in [
            (SocketOption::ReceiveBuffer as fn(usize) -> SocketOption, SocketQuery::ReceiveBuffer, sysctl("rmem_max")),
            (SocketOption::SendBuffer as fn(usize) -> SocketOption, SocketQuery::SendBuffer, sysctl("wmem_max")),
        ] {
            s.set_option(set(65_536)).expect("64 KiB");
            assert_eq!(bytes(&mut s, query), 131_072, "{kind:?} {query:?}: 64 KiB is stored doubled");
            s.set_option(set(max * 4)).expect("past the maximum");
            assert_eq!(bytes(&mut s, query), 2 * max, "{kind:?} {query:?}: clamped to the max, doubled");
        }
    }
}

/// **`IPV6_V6ONLY` starts at the host's `net.ipv6.bindv6only`** -- 0 on this host, so a v6 socket
/// carries v4 traffic until told otherwise -- and a v4-mapped peer really does reach one.
#[test]
fn a_v6_socket_starts_dual_stack_as_the_host_configures_and_carries_v4() {
    let default = std::fs::read_to_string("/proc/sys/net/ipv6/bindv6only").expect("the sysctl");
    let mut six = socket(SocketKind::Datagram, IpFamily::V6);
    assert_eq!(
        six.get_option(SocketQuery::V6Only).expect("IPV6_V6ONLY"),
        OptionValue::Flag(default.trim() == "1"),
        "a fresh socket reads the host's default"
    );
    six.set_option(SocketOption::V6Only(false)).expect("dual stack");
    six.bind(&SocketAddress::unspecified(IpFamily::V6)).expect("bind [::]:0");
    let port = six.local_address().expect("bound").port();
    let sender = HostDatagram::bind("127.0.0.1:0").expect("a v4 socket");
    sender.send_to(b"v4", ("127.0.0.1", port)).expect("send to the v6 socket over v4");
    wait_until(&six, Interest::READABLE, "the v4 datagram", |r| r.readable);
    let mut buf = [0u8; 8];
    let (n, from) = six.recv_from(&mut buf).expect("recvfrom");
    assert_eq!(&buf[..n], b"v4");
    assert_eq!(from.family(), IpFamily::V6, "a v4 peer on a v6 socket is a mapped address");
}

/// **A reset from the peer is `POLLERR | POLLHUP`, unasked, and the next `recv` is
/// `ECONNRESET`.** The peer closes with `SO_LINGER {1, 0}`; this side asks only about
/// readability, so the two bits it gets back are ones `poll(2)` reports whatever was asked --
/// the half a backend that masked `revents` by the interest, or dropped `POLLHUP`, would lose.
#[test]
fn a_peers_reset_is_reported_as_error_and_hangup_without_being_asked_for() {
    let (listener, address) = listening(IpFamily::V4);
    let mut client = socket(SocketKind::Stream, IpFamily::V4);
    let _ = client.connect(&address).expect("connect");
    wait_until(&client, Interest::WRITABLE, "the connect", |r| r.writable || r.error);
    assert_eq!(client.connect_result().expect("result"), ConnectOutcome::Connected);
    let (peer, _) = listener.accept().expect("accept");
    // The peer is `std`'s; its abortive close is set through `libc` on its descriptor.
    use std::os::fd::AsRawFd;
    let linger = libc::linger { l_onoff: 1, l_linger: 0 };
    // SAFETY: a live descriptor, and a `struct linger` of the size given.
    let rc = unsafe {
        libc::setsockopt(
            peer.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&raw const linger).cast(),
            core::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "SO_LINGER on the peer");
    drop(peer);
    let reset = wait_until(&client, Interest::READABLE, "the reset", |r| r.error);
    assert!(reset.hangup, "a reset connection is POLLHUP as well: {reset:?}");
    let mut buf = [0u8; 4];
    assert_eq!(client.recv(&mut buf).unwrap_err().kind(), Some(NetErrorKind::ConnectionReset));
}

/// **A stream closed in both directions is `POLLHUP`**, and one closed only by the peer is not:
/// after the peer's FIN the socket is readable (end of file) and not hung up; after this side's
/// own `shutdown(Write)` as well, `sk_shutdown` is `SHUTDOWN_MASK` and Linux reports hang-up.
#[test]
fn a_stream_shut_in_both_directions_reports_hangup_and_one_direction_does_not() {
    let (listener, address) = listening(IpFamily::V4);
    let mut client = socket(SocketKind::Stream, IpFamily::V4);
    let _ = client.connect(&address).expect("connect");
    wait_until(&client, Interest::WRITABLE, "the connect", |r| r.writable || r.error);
    assert_eq!(client.connect_result().expect("result"), ConnectOutcome::Connected);
    let (peer, _) = listener.accept().expect("accept");
    let before = client.readiness().expect("readiness");
    assert!(!before.hangup && !before.readable, "an open idle stream: {before:?}");
    drop(peer);
    let half = wait_until(&client, Interest::READABLE, "the peer's FIN", |r| r.readable);
    assert!(!half.hangup, "the peer closed and this side has not: not yet a hang-up: {half:?}");
    client.shutdown(Shutdown::Write).expect("SHUT_WR");
    let both = wait_until(&client, Interest::READABLE, "both directions shut", |r| r.hangup);
    assert!(both.readable, "and still readable, at end of file: {both:?}");
}

/// **A connect whose SYN is dropped stays in progress, and a second `connect` is the kernel's
/// `EALREADY`, which is `InProgress`** -- the row of `start_connect`'s table the ordinary tests
/// cannot reach, because a loopback handshake finishes too fast to call `connect` twice inside it.
///
/// The SYN is dropped by filling a listener's accept queue: `listen(0)` admits one connection
/// that nobody accepts, and Linux drops the next SYN while the queue is full, so that connect sits
/// in `SYN_SENT`. A connect that has **finished** answers `EISCONN` to a second call, which is
/// `Connected`.
#[test]
fn a_second_connect_while_the_first_is_pending_is_in_progress_and_after_it_is_connected() {
    let mut listener = Socket::new(SocketKind::Stream, IpFamily::V4, loopback_policy()).expect("s");
    listener.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind");
    listener.listen(0).expect("listen(0)");
    let address = listener.local_address().expect("bound");

    let mut first = socket(SocketKind::Stream, IpFamily::V4);
    let _ = first.connect(&address).expect("the one the queue admits");
    wait_until(&first, Interest::WRITABLE, "the first connect", |r| r.writable || r.error);
    assert_eq!(first.connect_result().expect("result"), ConnectOutcome::Connected);
    assert_eq!(
        first.connect(&address).expect("again, once connected"),
        ConnectProgress::Connected,
        "EISCONN is Connected"
    );

    let mut second = socket(SocketKind::Stream, IpFamily::V4);
    assert_eq!(
        second.connect(&address).expect("a connect the queue will not admit"),
        ConnectProgress::InProgress,
        "a non-blocking connect answers EINPROGRESS"
    );
    std::thread::sleep(Duration::from_millis(200));
    let pending = second.readiness().expect("readiness");
    assert!(!pending.writable && !pending.error, "its SYN was dropped: {pending:?}");
    assert_eq!(second.connect_result().expect("result"), ConnectOutcome::InProgress);
    assert_eq!(
        second.connect(&address).expect("a second connect while pending"),
        ConnectProgress::InProgress,
        "EALREADY is InProgress, not an error"
    );
}

/// **The kernel's own range for the keep-alive options is reported, not swallowed**: `TCP_KEEPIDLE`
/// and `TCP_KEEPINTVL` take `1..=32767` seconds and `TCP_KEEPCNT` `1..=127`, and outside that
/// Linux answers `EINVAL` -- which reaches the caller as `InvalidInput`. The edges inside the
/// range are accepted and read back.
#[test]
fn the_kernels_keep_alive_ranges_are_reported_as_invalid_input() {
    let mut s = socket(SocketKind::Stream, IpFamily::V4);
    for bad in [
        SocketOption::KeepAliveIdle(Duration::ZERO),
        SocketOption::KeepAliveIdle(Duration::from_secs(32_768)),
        SocketOption::KeepAliveInterval(Duration::ZERO),
        SocketOption::KeepAliveCount(0),
        SocketOption::KeepAliveCount(128),
    ] {
        let error = s.set_option(bad).expect_err("outside the kernel's range");
        assert_eq!(error.kind(), Some(NetErrorKind::InvalidInput), "{bad:?}: {error}");
    }
    s.set_option(SocketOption::KeepAliveIdle(Duration::from_secs(32_767))).expect("the top");
    s.set_option(SocketOption::KeepAliveCount(127)).expect("the top");
    assert_eq!(
        s.get_option(SocketQuery::KeepAliveIdle).expect("read"),
        OptionValue::Interval(Duration::from_secs(32_767))
    );
    assert_eq!(s.get_option(SocketQuery::KeepAliveCount).expect("read"), OptionValue::Count(127));
}

/// **A blocking socket's connect finishes inside the call**: the seam's sockets start blocking
/// (`socket(2)`'s default and the seam's contract), and a blocking loopback connect answers
/// `Connected` rather than the non-blocking `InProgress`.
#[test]
fn a_blocking_sockets_connect_is_connected_when_it_returns() {
    let (listener, address) = listening(IpFamily::V4);
    let mut client = Socket::new(SocketKind::Stream, IpFamily::V4, loopback_policy()).expect("s");
    assert!(!client.nonblocking());
    assert_eq!(client.connect(&address).expect("connect"), ConnectProgress::Connected);
    let (mut peer, _) = listener.accept().expect("accept");
    client.send(b"ok").expect("send");
    let mut buf = [0u8; 2];
    peer.read_exact(&mut buf).expect("read");
    assert_eq!(&buf, b"ok");
}

/// **An idle poll waits out its timeout, and not much longer**: a bound socket nothing sends to,
/// asked for readability for 150 ms, answers 0 after at least 150 ms. Every other test here polls
/// in a loop until something happens, so a `poll` that ignored its timeout would pass them all by
/// spinning; this is the one that says the wait happens. The upper bound is generous (1 s) and
/// only catches a wait that never ends on its own.
#[test]
fn an_idle_poll_waits_its_timeout_and_returns_nothing() {
    let mut idle = socket(SocketKind::Datagram, IpFamily::V4);
    idle.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind");
    let mut entries = [PollEntry::new(&idle, Interest::READABLE)];
    let started = Instant::now();
    let ready = poll(&mut entries, Duration::from_millis(150)).expect("poll");
    let waited = started.elapsed();
    assert_eq!(ready, 0);
    assert!(waited >= Duration::from_millis(150), "an idle poll returned after {waited:?}");
    assert!(waited < Duration::from_secs(1), "{waited:?}");
}
