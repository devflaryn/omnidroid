//! Loopback private per (instance, user) (`omni_linux::loopns`, `crate::hostnet`): a guest server
//! on 127.0.0.1 is reached by its own namespace, refused to another -- which may take the same
//! port -- and a real host listener cannot be named at all unless it is exposed on purpose. A host
//! program that names a guest server's real host port is dropped by the guest's accept and recv
//! filters.
mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use omni_linux::loopns::{expose, Proto};
use omni_linux::ExitStatus;

type Run = Option<(ExitStatus, String, String)>;

fn instance(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("omni-loopns-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A guest server in `u0` (uid 10000), running beside the test for at most 20 s. Returns once its
/// namespace entries exist -- it has bound -- with the thread to join for its own checks.
fn server(inst: &std::path::Path, port: &'static str) -> std::thread::JoinHandle<Run> {
    let inst_owned = inst.to_path_buf();
    let h = std::thread::spawn(move || common::run_fixture_as(&inst_owned, "loopiso", &["server", port], 10_000));
    let dir = inst.join(".omni-loopback/u0");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !(dir.join(format!("f-tcp-{port}")).exists() && dir.join(format!("f-udp-{port}")).exists()) {
        assert!(Instant::now() < deadline, "the guest server never bound");
        assert!(!h.is_finished(), "the guest server exited before binding");
        std::thread::sleep(Duration::from_millis(50));
    }
    h
}

/// The host port a guest server's `f-<proto>-<guest>` entry names.
fn host_port(inst: &std::path::Path, proto: &str, guest: &str) -> u16 {
    let text = std::fs::read_to_string(inst.join(format!(".omni-loopback/u0/f-{proto}-{guest}"))).expect("entry");
    text.split_whitespace().next().unwrap().parse().unwrap()
}

/// The server's own checks, after its clients are done.
fn server_ok(h: std::thread::JoinHandle<Run>) {
    let Some((_, out, err)) = h.join().unwrap() else { return };
    assert!(!out.contains("FAIL"), "server: {out}\n{err}");
    assert!(out.contains("ok getsockname shows the guest port"), "server: {out}\n{err}");
}

#[test]
fn a_namespace_reaches_its_own_server() {
    let inst = instance("own");
    let srv = server(&inst, "47100");
    let Some((status, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", "47100", "reach"], 10_001) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 3, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0));
    server_ok(srv);
}

#[test]
fn another_users_namespace_is_refused_and_may_take_the_same_port() {
    let inst = instance("other");
    let srv = server(&inst, "47101");
    // User 10's app: another namespace.
    let Some((status, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", "47101", "refused"], 1_010_000) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 15, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0));
    server_ok(srv);
}

#[test]
fn a_real_host_listener_cannot_be_named_unless_exposed() {
    let inst = instance("host");
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut c = c;
            let _ = c.write_all(b"hi");
        }
    });
    // A host datagram echo on the same port number, for the UDP checks to have something to hit.
    let echo = UdpSocket::bind(("127.0.0.1", port)).expect("a host datagram echo on the same port");
    std::thread::spawn(move || {
        let mut b = [0u8; 64];
        while let Ok((n, from)) = echo.recv_from(&mut b) {
            let _ = echo.send_to(&b[..n], from);
        }
    });
    let p = port.to_string();
    let Some((_, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", &p, "refused"], 10_000) else { return };
    for check in ["ok tcp: refused", "ok tcp to 0.0.0.0: refused", "ok udp: nothing comes back", "ok udp to 0.0.0.0: nothing comes back"] {
        assert!(out.contains(check), "a host port is not reachable ({check}): {out}\n{err}");
    }
    let _exposed = expose(&inst, "u0", Proto::Tcp, port, port).expect("expose");
    let Some((_, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", &p, "reach"], 10_000) else { return };
    assert!(out.contains("ok tcp: reaches the server"), "exposed, it is: {out}\n{err}");
}

#[test]
fn a_host_program_cannot_reach_a_guest_server() {
    let inst = instance("hostprog");
    // One guest server per probe: the TCP probe leaves its server in a blocking accept.
    let _srv_tcp = server(&inst, "47102");
    let srv_udp = server(&inst, "47103");
    // Its host ports are real, and a host program can name them: the guest drops what does not
    // come from its namespace (the accept and recv filters).
    let tcp = host_port(&inst, "tcp", "47102");
    let mut c = TcpStream::connect(("127.0.0.1", tcp)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut got = [0u8; 2];
    match c.read(&mut got) {
        Ok(0) => {} // closed by the guest's accept filter
        Ok(_) => panic!("the guest answered a host program: {got:?}"),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted) => {}
        Err(e) => panic!("the guest never accepted the host connection (its accept filter did not run): {e}"),
    }
    let udp = host_port(&inst, "udp", "47103");
    let u = UdpSocket::bind("127.0.0.1:0").unwrap();
    u.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    u.send_to(b"host", ("127.0.0.1", udp)).unwrap();
    let mut b = [0u8; 16];
    assert!(u.recv_from(&mut b).is_err(), "a host datagram was answered by a guest server");
    // The datagram server was alive and serving throughout (a stuck one would prove nothing): its
    // own namespace still gets its echo.
    let Some((_, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", "47103", "reach"], 10_001) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert!(out.contains("ok udp reply to an unbound client"), "the udp server was not serving: {out}\n{err}");
    // What the server itself received: its namespace's datagram, never the host program's. (The
    // server's echo to a host port would be dropped anyway, so only its own log tells.)
    let Some((_, sout, serr)) = srv_udp.join().unwrap() else { return };
    assert!(sout.contains("udp got ping"), "the udp server saw no datagram: {sout}\n{serr}");
    assert!(!sout.contains("udp got host"), "the recv filter let a host program's datagram in: {sout}\n{serr}");
}

#[test]
fn ports_translate_for_dual_stack_and_a_held_port_is_in_use() {
    let inst = instance("self");
    let Some((status, out, err)) = common::run_fixture_as(&inst, "loopiso", &["self"], 10_000) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 13, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0));
}
