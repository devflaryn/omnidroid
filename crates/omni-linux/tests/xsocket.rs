//! An abstract unix socket listened on in one host process is connected to from another host
//! process of the same instance (`crate::xsocket`): the WebView zygote, started in a host process
//! of its own, listens on `@com.android.internal.os.WebViewZygoteInit/<uuid>`, and
//! ActivityManager's connect from the system's host process was ENOENT -- retried 2,402 times
//! with its locks held, until an app was killed for not answering input (r10).
//!
//! The test runs itself a second time as the server's host process.
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Binds, Sysroot, Vfs}};

const NAME: &[u8] = b"\0omni.xsocket.test";

fn process(instance: &Path) -> (Arc<Process>, omni_linux::Task) {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec()).with_binds(Binds::of(instance));
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    (p, t)
}

/// A `sockaddr_un` for the abstract name, written at `at`; its length.
fn address(p: &Process, at: u64) -> u64 {
    let mut a = 1u16.to_le_bytes().to_vec(); // AF_UNIX
    a.extend_from_slice(NAME);
    p.mem.write(at, &a).unwrap();
    a.len() as u64
}

/// The server's host process: listen, then echo each connection's first message back.
fn serve(instance: &Path) {
    let (p, mut t) = process(instance);
    let s = p.scratch();
    let fd = p.syscall(&mut t, nr::SOCKET, [1, 1, 0, 0, 0, 0]);
    let len = address(&p, s);
    assert_eq!(p.syscall(&mut t, nr::BIND, [fd, s, len, 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::LISTEN, [fd, 8, 0, 0, 0, 0]), 0);
    println!("ready");
    let conn = p.syscall(&mut t, nr::ACCEPT4, [fd, 0, 0, 0, 0, 0]) as i64;
    assert!(conn >= 0, "accept: {conn}");
    let buf = s + 0x1000;
    let n = p.syscall(&mut t, nr::RECVFROM, [conn as u64, buf, 64, 0, 0, 0]) as i64;
    assert!(n > 0, "recv: {n}");
    let got = p.mem.read(buf, n as usize).unwrap();
    let mut reply = b"echo:".to_vec();
    reply.extend_from_slice(&got);
    p.mem.write(buf, &reply).unwrap();
    assert_eq!(p.syscall(&mut t, nr::SENDTO, [conn as u64, buf, reply.len() as u64, 0, 0, 0]) as usize, reply.len());
    std::thread::sleep(std::time::Duration::from_secs(2));
}

#[test]
fn an_abstract_socket_is_reached_from_another_host_process() {
    if let Ok(dir) = std::env::var("OMNI_XSOCKET_SERVE") {
        serve(Path::new(&dir));
        return;
    }
    let instance = std::env::temp_dir().join(format!("omni-xsocket-{}", std::process::id()));
    std::fs::create_dir_all(&instance).unwrap();
    let mut server = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "an_abstract_socket_is_reached_from_another_host_process", "--nocapture"])
        .env("OMNI_XSOCKET_SERVE", &instance)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(server.stdout.take().unwrap()).lines();
    assert!(lines.any(|l| l.is_ok_and(|l| l.contains("ready"))), "the server never listened");

    let (p, mut t) = process(&instance);
    let s = p.scratch();
    let fd = p.syscall(&mut t, nr::SOCKET, [1, 1, 0, 0, 0, 0]);
    let len = address(&p, s);
    assert_eq!(p.syscall(&mut t, nr::CONNECT, [fd, s, len, 0, 0, 0]) as i64, 0, "connect to the other host process's socket");
    let buf = s + 0x1000;
    p.mem.write(buf, b"hello").unwrap();
    assert_eq!(p.syscall(&mut t, nr::SENDTO, [fd, buf, 5, 0, 0, 0]), 5);
    let n = p.syscall(&mut t, nr::RECVFROM, [fd, buf, 64, 0, 0, 0]) as i64;
    assert_eq!(p.mem.read(buf, n.max(0) as usize).unwrap(), b"echo:hello", "the reply came back across");
    let _ = server.wait();
    let _ = std::fs::remove_dir_all(&instance);
}
