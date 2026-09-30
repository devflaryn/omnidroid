//! Internet sockets on the host's network (`crate::hostnet`): a guest program (`fixtures/inetnet`,
//! bionic's own socket calls) talks to servers this test runs on the host's loopback -- a TCP echo
//! server, one that speaks late, a port nothing listens on, a UDP echo socket -- and to a server of
//! its own.
mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::time::Duration;

use omni_linux::ExitStatus;

/// Echo every connection's bytes back until it closes its end, then close.
fn echo_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(mut c) = c else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = c.read(&mut buf) {
                    if n == 0 || c.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

/// Send "late" 700 ms after a connection arrives, then hold it open a while.
fn late_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(mut c) = c else { continue };
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(700));
                let _ = c.write_all(b"late");
                std::thread::sleep(Duration::from_secs(5));
            });
        }
    });
    port
}

fn closed_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn udp_echo() -> u16 {
    let u = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = u.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = u.recv_from(&mut buf) {
            let _ = u.send_to(&buf[..n], from);
        }
    });
    port
}

#[test]
fn a_guest_talks_tcp_and_udp_over_the_hosts_network() {
    use omni_linux::loopns::{expose, Proto};
    let (echo, late, closed, udp) = (echo_server(), late_server(), closed_port(), udp_echo());
    // The host's servers stand in for the network's: a guest's loopback is its namespace's
    // (`crate::loopns`), so each is handed to the fixture's namespace -- an app's, user 0's -- on
    // purpose, as the same port. The closed port is not: refused either way.
    let instance = std::env::temp_dir().join(format!("omni-linux-hostnet-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let _held = [
        expose(&instance, "u0", Proto::Tcp, echo, echo).expect("echo"),
        expose(&instance, "u0", Proto::Tcp, late, late).expect("late"),
        expose(&instance, "u0", Proto::Udp, udp, udp).expect("udp"),
    ];
    let ports = [echo, late, closed, udp].map(|p| p.to_string());
    let args: Vec<&str> = ports.iter().map(String::as_str).collect();
    let Some((status, out, err)) = common::run_fixture_as(&instance, "inetnet", &args, 10_000) else { return };
    eprintln!("{out}");
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 37, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
}
