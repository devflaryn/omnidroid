//! Sockets: `logd`'s write socket prints what the guest logs; any other address is refused as a
//! device without that service refuses it.
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const AF_UNIX: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_STREAM: u64 = 1;
const SOCK_CLOEXEC: u64 = 0o2000000;
const ENOENT: i64 = -2;

fn process() -> (Arc<Process>, omni_linux::Task, u64, Arc<parking_lot::Mutex<Vec<u8>>>) {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let p = Process::for_tests(vfs, Output::Capture(Arc::clone(&out)));
    let t = p.test_task();
    let s = p.scratch();
    (p, t, s, out)
}

fn sockaddr_un(p: &Process, at: u64, path: &str) -> u64 {
    let mut b = (AF_UNIX as u16).to_le_bytes().to_vec();
    b.extend_from_slice(path.as_bytes());
    b.push(0);
    p.mem.write(at, &b).unwrap();
    b.len() as u64
}

#[test]
fn a_log_written_to_logdw_is_printed_with_its_priority_and_tag() {
    let (p, mut t, s, out) = process();
    let fd = p.syscall(&mut t, nr::SOCKET, [AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, 0, 0, 0]) as i64;
    assert!(fd >= 3, "{fd}");
    let len = sockaddr_un(&p, s, "/dev/socket/logdw");
    assert_eq!(p.syscall(&mut t, nr::CONNECT, [fd as u64, s, len, 0, 0, 0]), 0);
    // liblog's packet: header { id u8, tid u16, sec u32, nsec u32 }, then priority, tag, message.
    let mut header = vec![0u8]; // LOG_ID_MAIN
    header.extend_from_slice(&1234u16.to_le_bytes());
    header.extend_from_slice(&[0; 8]);
    let parts: [&[u8]; 4] = [&header, &[6], b"art\0", b"Runtime aborting\0"];
    let mut at = s + 256;
    for (i, part) in parts.iter().enumerate() {
        p.mem.write(at, part).unwrap();
        p.mem.write_u64(s + 1024 + i as u64 * 16, at).unwrap();
        p.mem.write_u64(s + 1024 + i as u64 * 16 + 8, part.len() as u64).unwrap();
        at += 64;
    }
    let total: usize = parts.iter().map(|x| x.len()).sum();
    assert_eq!(p.syscall(&mut t, nr::WRITEV, [fd as u64, s + 1024, 4, 0, 0, 0]), total as u64);
    let text = String::from_utf8_lossy(&out.lock()).into_owned();
    assert!(text.contains("E/art") && text.contains("Runtime aborting"), "{text:?}");
}

#[test]
fn a_socket_with_no_service_behind_it_is_refused_at_connect() {
    let (p, mut t, s, _) = process();
    let fd = p.syscall(&mut t, nr::SOCKET, [AF_UNIX, SOCK_STREAM, 0, 0, 0, 0]) as i64;
    assert!(fd >= 3);
    let len = sockaddr_un(&p, s, "/dev/socket/statsdw");
    assert_eq!(p.syscall(&mut t, nr::CONNECT, [fd as u64, s, len, 0, 0, 0]) as i64, ENOENT);
    assert_eq!(p.syscall(&mut t, nr::CLOSE, [fd as u64, 0, 0, 0, 0, 0]), 0);
}
