//! `socketpair(AF_UNIX)`: two connected ends, as SurfaceFlinger's `BitTube` (vsync events) and
//! an app's `InputChannel` (input events) make them -- `SOCK_SEQPACKET` keeps each message whole,
//! `SOCK_STREAM` is bytes; an end is readable when the other has sent, and hung up when the other
//! is closed.
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const SOCK_SEQPACKET: u64 = 5;
const SOCK_NONBLOCK: u64 = 0o4000;
const EAGAIN: i64 = -11;
const POLLIN: u16 = 1;
const POLLHUP: u16 = 0x10;

fn process() -> (Arc<Process>, omni_linux::Task, u64) {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    let s = p.scratch();
    (p, t, s)
}

fn pair(p: &Arc<Process>, t: &mut omni_linux::Task, s: u64, ty: u64) -> (u64, u64) {
    assert_eq!(p.syscall(t, nr::SOCKETPAIR, [AF_UNIX, ty, 0, s, 0, 0]), 0, "socketpair");
    let b = p.mem.read(s, 8).unwrap();
    (u64::from(u32::from_le_bytes(b[0..4].try_into().unwrap())), u64::from(u32::from_le_bytes(b[4..8].try_into().unwrap())))
}

fn send(p: &Arc<Process>, t: &mut omni_linux::Task, s: u64, fd: u64, bytes: &[u8]) -> i64 {
    p.mem.write(s + 0x100, bytes).unwrap();
    p.syscall(t, nr::SENDTO, [fd, s + 0x100, bytes.len() as u64, 0, 0, 0]) as i64
}

fn recv(p: &Arc<Process>, t: &mut omni_linux::Task, s: u64, fd: u64, len: u64) -> Result<Vec<u8>, i64> {
    let n = p.syscall(t, nr::RECVFROM, [fd, s + 0x200, len, 0, 0, 0]) as i64;
    if n < 0 { Err(n) } else { Ok(p.mem.read(s + 0x200, n as usize).unwrap()) }
}

fn poll1(p: &Arc<Process>, t: &mut omni_linux::Task, s: u64, fd: u64) -> u16 {
    let mut pfd = (fd as i32).to_le_bytes().to_vec();
    pfd.extend_from_slice(&(POLLIN | 4).to_le_bytes());
    pfd.extend_from_slice(&0u16.to_le_bytes());
    p.mem.write(s + 0x300, &pfd).unwrap();
    let mut ts = [0u8; 16]; // a zero timeout: report, do not wait
    ts[0] = 0;
    p.mem.write(s + 0x320, &ts).unwrap();
    assert!(p.syscall(t, nr::PPOLL, [s + 0x300, 1, s + 0x320, 0, 0, 0]) as i64 >= 0);
    u16::from_le_bytes(p.mem.read(s + 0x306, 2).unwrap().try_into().unwrap())
}

#[test]
fn seqpacket_keeps_each_message_whole() {
    let (p, mut t, s) = process();
    let (a, b) = pair(&p, &mut t, s, SOCK_SEQPACKET | SOCK_NONBLOCK);
    assert_eq!(recv(&p, &mut t, s, b, 64), Err(EAGAIN), "nothing sent yet");
    assert_eq!(poll1(&p, &mut t, s, b) & POLLIN, 0);
    assert_eq!(send(&p, &mut t, s, a, b"vsync-1"), 7);
    assert_eq!(send(&p, &mut t, s, a, b"vsync-two"), 9);
    assert_ne!(poll1(&p, &mut t, s, b) & POLLIN, 0, "readable once sent");
    assert_eq!(recv(&p, &mut t, s, b, 64).unwrap(), b"vsync-1", "one message, not both");
    assert_eq!(recv(&p, &mut t, s, b, 5).unwrap(), b"vsync", "a short read truncates the message");
    assert_eq!(recv(&p, &mut t, s, b, 64), Err(EAGAIN), "and its rest is gone");
    // The other way round.
    assert_eq!(send(&p, &mut t, s, b, b"back"), 4);
    assert_eq!(recv(&p, &mut t, s, a, 64).unwrap(), b"back");
    // Closing one end hangs up the other: end of file, and POLLHUP.
    assert_eq!(p.syscall(&mut t, nr::CLOSE, [a, 0, 0, 0, 0, 0]), 0);
    assert_ne!(poll1(&p, &mut t, s, b) & POLLHUP, 0, "hung up");
    assert_eq!(recv(&p, &mut t, s, b, 64).unwrap(), b"", "end of file");
}

#[test]
fn stream_is_bytes() {
    let (p, mut t, s) = process();
    let (a, b) = pair(&p, &mut t, s, SOCK_STREAM | SOCK_NONBLOCK);
    assert_eq!(send(&p, &mut t, s, a, b"abc"), 3);
    assert_eq!(send(&p, &mut t, s, a, b"def"), 3);
    assert_eq!(recv(&p, &mut t, s, b, 4).unwrap(), b"abcd");
    assert_eq!(recv(&p, &mut t, s, b, 64).unwrap(), b"ef");
}
