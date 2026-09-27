//! `pipe2`: bytes written to one end are read from the other in order; a read of an empty pipe
//! waits for a writer (or answers EAGAIN when non-blocking) and sees end-of-file once every write
//! end is closed; a write with no reader left is EPIPE.
use std::sync::Arc;
use std::time::Duration;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const O_NONBLOCK: u64 = 0o4000;
const EAGAIN: i64 = -11;
const EPIPE: i64 = -32;

fn process() -> (Arc<Process>, omni_linux::Task, u64) {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    let s = p.scratch();
    (p, t, s)
}

fn fds(p: &Process, s: u64) -> (u64, u64) {
    let b = p.mem.read(s, 8).unwrap();
    (u64::from(u32::from_le_bytes(b[0..4].try_into().unwrap())), u64::from(u32::from_le_bytes(b[4..8].try_into().unwrap())))
}

#[test]
fn bytes_go_through_in_order_and_eof_follows_the_last_writer() {
    let (p, mut t, s) = process();
    assert_eq!(p.syscall(&mut t, nr::PIPE2, [s, 0, 0, 0, 0, 0]), 0);
    let (r, w) = fds(&p, s);
    p.mem.write(s + 64, b"hello pipe").unwrap();
    assert_eq!(p.syscall(&mut t, nr::WRITE, [w, s + 64, 5, 0, 0, 0]), 5);
    assert_eq!(p.syscall(&mut t, nr::WRITE, [w, s + 69, 5, 0, 0, 0]), 5);
    assert_eq!(p.syscall(&mut t, nr::READ, [r, s + 128, 64, 0, 0, 0]), 10);
    assert_eq!(p.mem.read(s + 128, 10).unwrap(), b"hello pipe");
    assert_eq!(p.syscall(&mut t, nr::CLOSE, [w, 0, 0, 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::READ, [r, s + 128, 64, 0, 0, 0]), 0, "end of file");
}

#[test]
fn an_empty_nonblocking_pipe_is_eagain_and_a_blocking_one_waits_for_a_writer() {
    let (p, mut t, s) = process();
    assert_eq!(p.syscall(&mut t, nr::PIPE2, [s, O_NONBLOCK, 0, 0, 0, 0]), 0);
    let (r, _w) = fds(&p, s);
    assert_eq!(p.syscall(&mut t, nr::READ, [r, s + 128, 64, 0, 0, 0]) as i64, EAGAIN);

    assert_eq!(p.syscall(&mut t, nr::PIPE2, [s, 0, 0, 0, 0, 0]), 0);
    let (r, w) = fds(&p, s);
    let writer = {
        let p = Arc::clone(&p);
        let mut t2 = p.test_task();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            p.mem.write(s + 256, b"late").unwrap();
            p.syscall(&mut t2, nr::WRITE, [w, s + 256, 4, 0, 0, 0])
        })
    };
    assert_eq!(p.syscall(&mut t, nr::READ, [r, s + 128, 64, 0, 0, 0]), 4);
    assert_eq!(writer.join().unwrap(), 4);
}

#[test]
fn a_write_with_no_reader_is_epipe() {
    let (p, mut t, s) = process();
    assert_eq!(p.syscall(&mut t, nr::PIPE2, [s, 0, 0, 0, 0, 0]), 0);
    let (r, w) = fds(&p, s);
    assert_eq!(p.syscall(&mut t, nr::CLOSE, [r, 0, 0, 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::WRITE, [w, s, 1, 0, 0, 0]) as i64, EPIPE);
}
