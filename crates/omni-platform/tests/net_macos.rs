//! macOS-only behaviour of the network seam that `net_loopback.rs` cannot see.
//!
//! **`SIGPIPE`.** A write to a socket whose peer has gone raises `SIGPIPE` on macOS, and the default
//! disposition kills the process with no error and no unwind. A Rust *test* binary cannot show
//! that happening: the Rust runtime sets `SIGPIPE` to `SIG_IGN` before `main`, so every test here
//! would pass whether or not the seam protected its sockets. An embedding is not obliged to be a
//! Rust program with that default. So each case runs in a **child** of this binary that first puts
//! `SIGPIPE` back to `SIG_DFL`, then writes to a dead connection, and must survive to report the
//! error -- the property is "the process is still alive", which only another process can observe.
#![cfg(target_os = "macos")]

use std::io::Read;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_platform::net::{
    IpFamily, NetErrorKind, NetPolicy, Socket, SocketAddress, SocketKind,
};

const CHILD: &str = "OMNI_NET_MACOS_SIGPIPE_CHILD";

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}
const SIGPIPE: i32 = 13;
const SIG_DFL: usize = 0;

fn restore_default_sigpipe() {
    // SAFETY: installs the default disposition for one signal; no handler code of ours runs.
    unsafe { signal(SIGPIPE, SIG_DFL) };
}

/// Write until the host reports the dead peer, and return what it reported.
fn write_until_refused(socket: &Socket) -> NetErrorKind {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match socket.send(&[0u8; 4096]) {
            Ok(_) => {}
            Err(error) => return error.kind().expect("an I/O failure has a kind"),
        }
        assert!(Instant::now() < deadline, "5 s of writes to a closed peer never failed");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn run_child(case: &str) -> std::process::Output {
    std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args([case, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, case)
        .output()
        .expect("run the child")
}

fn assert_survived(case: &str) {
    use std::os::unix::process::ExitStatusExt;
    let output = run_child(case);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.signal(),
        None,
        "the child was killed by signal {:?} (13 is SIGPIPE): {stdout}",
        output.status.signal()
    );
    assert!(output.status.success(), "the child failed: {stdout}{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("SURVIVED"), "the child never reported: {stdout}");
}

/// A socket this seam **created** survives writing to a peer that closed.
#[test]
fn a_created_socket_writing_to_a_closed_peer_gets_an_error_not_sigpipe() {
    if std::env::var(CHILD).as_deref() == Ok("a_created_socket_writing_to_a_closed_peer_gets_an_error_not_sigpipe") {
        restore_default_sigpipe();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = SocketAddress::from_std(listener.local_addr().expect("address"));
        let mut client = Socket::new(SocketKind::Stream, IpFamily::V4, Arc::new(NetPolicy::loopback_only()))
            .expect("socket");
        client.connect(&address).expect("connect");
        let (peer, _) = listener.accept().expect("accept");
        drop(peer);
        let kind = write_until_refused(&client);
        println!("SURVIVED with {kind:?}");
        assert!(matches!(kind, NetErrorKind::BrokenPipe | NetErrorKind::ConnectionReset), "{kind:?}");
        return;
    }
    assert_survived("a_created_socket_writing_to_a_closed_peer_gets_an_error_not_sigpipe");
}

/// A socket this seam **accepted** survives writing to a peer that closed.
#[test]
fn an_accepted_socket_writing_to_a_closed_peer_gets_an_error_not_sigpipe() {
    if std::env::var(CHILD).as_deref() == Ok("an_accepted_socket_writing_to_a_closed_peer_gets_an_error_not_sigpipe") {
        restore_default_sigpipe();
        let policy = Arc::new(NetPolicy::loopback_only());
        let mut listener = Socket::new(SocketKind::Stream, IpFamily::V4, policy).expect("socket");
        listener.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind");
        listener.listen(4).expect("listen");
        let address = listener.local_address().expect("address");
        let mut client = std::net::TcpStream::connect(address.to_std()).expect("connect");
        let (accepted, _) = listener.accept().expect("accept");
        let mut byte = [0u8; 1];
        client.set_nonblocking(true).expect("nonblocking");
        let _ = client.read(&mut byte);
        drop(client);
        let kind = write_until_refused(&accepted);
        println!("SURVIVED with {kind:?}");
        assert!(matches!(kind, NetErrorKind::BrokenPipe | NetErrorKind::ConnectionReset), "{kind:?}");
        return;
    }
    assert_survived("an_accepted_socket_writing_to_a_closed_peer_gets_an_error_not_sigpipe");
}

/// A new socket that lands on a just-freed descriptor must not read back the old socket's
/// path-MTU mode: the host's bit is off on a new socket, and "nothing was set" is the answer.
/// Fifty rounds, because descriptor reuse is the kernel's lowest-free rule and another test's
/// thread may take the number once or twice; one reuse among them is enough to catch the defect.
#[test]
fn a_new_socket_on_a_reused_descriptor_has_no_path_mtu_mode() {
    use omni_platform::net::{OptionValue, PathMtu, SocketOption, SocketQuery};
    let policy = Arc::new(NetPolicy::loopback_only());
    for round in 0..50 {
        let mut old = Socket::new(SocketKind::Datagram, IpFamily::V4, Arc::clone(&policy)).expect("socket");
        old.set_option(SocketOption::PathMtuDiscovery(PathMtu::Dont)).expect("set Dont");
        drop(old);
        let mut new = Socket::new(SocketKind::Datagram, IpFamily::V4, Arc::clone(&policy)).expect("socket");
        assert_eq!(
            new.get_option(SocketQuery::PathMtuDiscovery).expect("query"),
            OptionValue::PathMtu(None),
            "round {round}: a fresh socket reported a mode nobody set on it"
        );
    }
}

/// A readiness wait shorter than a millisecond still waits: `poll(2)` counts in milliseconds, and
/// rounding 500 us down to 0 would turn every short timed wait into a spin. A lower bound only --
/// a wait can overrun, it cannot underrun.
#[test]
fn a_sub_millisecond_readiness_wait_is_not_a_spin() {
    use omni_platform::net::{poll, Interest, PollEntry};
    let mut socket = Socket::new(SocketKind::Datagram, IpFamily::V4, Arc::new(NetPolicy::loopback_only()))
        .expect("socket");
    socket.bind(&SocketAddress::loopback(IpFamily::V4, 0)).expect("bind");
    for _ in 0..5 {
        let mut entries = [PollEntry::new(&socket, Interest::READABLE)];
        let started = Instant::now();
        let ready = poll(&mut entries, Duration::from_micros(500)).expect("poll");
        let waited = started.elapsed();
        assert_eq!(ready, 0, "nothing was sent to it");
        assert!(waited >= Duration::from_micros(450), "a 500 us wait returned after {waited:?}");
    }
}
