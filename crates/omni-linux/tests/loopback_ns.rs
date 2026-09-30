//! Loopback private per (instance, user) (`omni_linux::loopns`, `crate::hostnet`): a guest server
//! on 127.0.0.1 is reached by its own namespace, refused to another -- which may take the same
//! port -- and a real host listener cannot be named at all unless it is exposed on purpose.
mod common;

use std::io::Write;
use std::net::TcpListener;
use std::time::Duration;

use omni_linux::loopns::{expose, Proto};
use omni_linux::ExitStatus;

fn instance(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("omni-loopns-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A guest server in `u0` (uid 10000), running beside the test for 20 s.
fn server(inst: &std::path::Path, port: &'static str) {
    let inst = inst.to_path_buf();
    std::thread::spawn(move || common::run_fixture_as(&inst, "loopiso", &["server", port], 10_000));
    std::thread::sleep(Duration::from_secs(3));
}

#[test]
fn a_namespace_reaches_its_own_server() {
    let inst = instance("own");
    server(&inst, "47100");
    let Some((status, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", "47100", "reach"], 10_001) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 3, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0));
}

#[test]
fn another_users_namespace_is_refused_and_may_take_the_same_port() {
    let inst = instance("other");
    server(&inst, "47101");
    // User 10's app: another namespace.
    let Some((status, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", "47101", "refused"], 1_010_000) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 3, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0));
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
    let p = port.to_string();
    let Some((_, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", &p, "refused"], 10_000) else { return };
    assert!(out.contains("ok tcp: refused"), "a host port is not reachable: {out}\n{err}");
    let _exposed = expose(&inst, "u0", Proto::Tcp, port, port).expect("expose");
    let Some((_, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", &p, "reach"], 10_000) else { return };
    assert!(out.contains("ok tcp: reaches the server"), "exposed, it is: {out}\n{err}");
}

#[test]
fn ports_translate_for_dual_stack_and_a_held_port_is_in_use() {
    let inst = instance("self");
    let Some((status, out, err)) = common::run_fixture_as(&inst, "loopiso", &["self"], 10_000) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 6, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0));
}
