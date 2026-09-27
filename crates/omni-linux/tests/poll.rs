//! `eventfd`, `timerfd` and `epoll`: what every `Looper` (ART's main thread, `servicemanager`,
//! `system_server`) waits in.
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const EFD_NONBLOCK: u64 = 0o4000;
const EFD_SEMAPHORE: u64 = 1;
const EAGAIN: i64 = -11;
const EPOLLIN: u32 = 1;
const EPOLLOUT: u32 = 4;
const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_DEL: u64 = 2;
const CLOCK_MONOTONIC: u64 = 1;

fn process() -> (Arc<Process>, omni_linux::Task, u64) {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    let s = p.scratch();
    (p, t, s)
}

fn write_u64(p: &Process, t: &mut omni_linux::Task, s: u64, fd: u64, v: u64) -> i64 {
    p.mem.write_u64(s, v).unwrap();
    p.syscall(t, nr::WRITE, [fd, s, 8, 0, 0, 0]) as i64
}

fn read_u64(p: &Process, t: &mut omni_linux::Task, s: u64, fd: u64) -> (i64, u64) {
    let r = p.syscall(t, nr::READ, [fd, s, 8, 0, 0, 0]) as i64;
    (r, p.mem.read_u64(s).unwrap())
}

#[test]
fn an_eventfd_counts_writes_and_a_read_takes_them() {
    let (p, mut t, s) = process();
    let fd = p.syscall(&mut t, nr::EVENTFD2, [0, EFD_NONBLOCK, 0, 0, 0, 0]);
    assert!((fd as i64) >= 0);
    assert_eq!(read_u64(&p, &mut t, s, fd).0, EAGAIN);
    assert_eq!(write_u64(&p, &mut t, s, fd, 3), 8);
    assert_eq!(write_u64(&p, &mut t, s, fd, 4), 8);
    assert_eq!(read_u64(&p, &mut t, s, fd), (8, 7));
    assert_eq!(read_u64(&p, &mut t, s, fd).0, EAGAIN);
    // A semaphore reads one at a time.
    let sem = p.syscall(&mut t, nr::EVENTFD2, [2, EFD_NONBLOCK | EFD_SEMAPHORE, 0, 0, 0, 0]);
    assert_eq!(read_u64(&p, &mut t, s, sem), (8, 1));
    assert_eq!(read_u64(&p, &mut t, s, sem), (8, 1));
    assert_eq!(read_u64(&p, &mut t, s, sem).0, EAGAIN);
}

fn epoll_add(p: &Process, t: &mut omni_linux::Task, s: u64, ep: u64, fd: u64, events: u32, data: u64) {
    let mut ev = events.to_le_bytes().to_vec();
    ev.extend_from_slice(&[0; 4]);
    ev.extend_from_slice(&data.to_le_bytes());
    p.mem.write(s + 512, &ev).unwrap();
    assert_eq!(p.syscall(t, nr::EPOLL_CTL, [ep, EPOLL_CTL_ADD, fd, s + 512, 0, 0]), 0);
}

/// `epoll_pwait` answers ready descriptors with their data; waits until one is ready; and times out.
#[test]
fn epoll_reports_what_is_ready_waits_for_the_rest_and_times_out() {
    let (p, mut t, s) = process();
    let ep = p.syscall(&mut t, nr::EPOLL_CREATE1, [0o2000000, 0, 0, 0, 0, 0]);
    let efd = p.syscall(&mut t, nr::EVENTFD2, [0, EFD_NONBLOCK, 0, 0, 0, 0]);
    epoll_add(&p, &mut t, s, ep, efd, EPOLLIN, 0xABCD);
    // Nothing ready: a 30 ms timeout.
    let start = Instant::now();
    assert_eq!(p.syscall(&mut t, nr::EPOLL_PWAIT, [ep, s + 1024, 8, 30, 0, 8]), 0);
    assert!(start.elapsed() >= Duration::from_millis(25));
    // Ready: the event and its data.
    assert_eq!(write_u64(&p, &mut t, s, efd, 1), 8);
    assert_eq!(p.syscall(&mut t, nr::EPOLL_PWAIT, [ep, s + 1024, 8, 0, 0, 8]), 1);
    let got = p.mem.read(s + 1024, 16).unwrap();
    assert_eq!(u32::from_le_bytes(got[0..4].try_into().unwrap()) & EPOLLIN, EPOLLIN);
    assert_eq!(u64::from_le_bytes(got[8..16].try_into().unwrap()), 0xABCD);
    // Waits until another thread writes.
    let _ = read_u64(&p, &mut t, s, efd);
    let writer = {
        let p = Arc::clone(&p);
        let mut t2 = p.test_task();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            p.mem.write_u64(s + 2048, 5).unwrap();
            p.syscall(&mut t2, nr::WRITE, [efd, s + 2048, 8, 0, 0, 0])
        })
    };
    assert_eq!(p.syscall(&mut t, nr::EPOLL_PWAIT, [ep, s + 1024, 8, u64::MAX, 0, 8]), 1, "-1 waits for ever");
    assert_eq!(writer.join().unwrap(), 8);
    // A pipe's write end is ready for writing; removed, it is not reported.
    assert_eq!(p.syscall(&mut t, nr::PIPE2, [s + 3000, 0, 0, 0, 0, 0]), 0);
    let w = u64::from(u32::from_le_bytes(p.mem.read(s + 3004, 4).unwrap().try_into().unwrap()));
    epoll_add(&p, &mut t, s, ep, w, EPOLLOUT, 7);
    assert_eq!(p.syscall(&mut t, nr::EPOLL_PWAIT, [ep, s + 1024, 8, 0, 0, 8]), 2);
    assert_eq!(p.syscall(&mut t, nr::EPOLL_CTL, [ep, EPOLL_CTL_DEL, w, 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::EPOLL_PWAIT, [ep, s + 1024, 8, 0, 0, 8]), 1);
}

/// A timerfd becomes readable when it expires, and a read answers the number of expirations.
#[test]
fn a_timerfd_expires_and_is_read_as_a_count() {
    let (p, mut t, s) = process();
    let fd = p.syscall(&mut t, nr::TIMERFD_CREATE, [CLOCK_MONOTONIC, EFD_NONBLOCK, 0, 0, 0, 0]);
    assert!((fd as i64) >= 0);
    // itimerspec { interval 10 ms, value 20 ms }
    for (i, v) in [0u64, 10_000_000, 0, 20_000_000].iter().enumerate() {
        p.mem.write_u64(s + i as u64 * 8, *v).unwrap();
    }
    assert_eq!(p.syscall(&mut t, nr::TIMERFD_SETTIME, [fd, 0, s, 0, 0, 0]), 0);
    assert_eq!(read_u64(&p, &mut t, s + 64, fd).0, EAGAIN, "not yet");
    let ep = p.syscall(&mut t, nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]);
    epoll_add(&p, &mut t, s, ep, fd, EPOLLIN, 1);
    assert_eq!(p.syscall(&mut t, nr::EPOLL_PWAIT, [ep, s + 1024, 8, 1000, 0, 8]), 1);
    std::thread::sleep(Duration::from_millis(35));
    let (r, n) = read_u64(&p, &mut t, s + 64, fd);
    assert_eq!(r, 8);
    assert!(n >= 2, "expired at 20 ms and again every 10 ms: {n}");
}

/// `ppoll` on an eventfd and a pipe.
#[test]
fn ppoll_reports_revents() {
    let (p, mut t, s) = process();
    let efd = p.syscall(&mut t, nr::EVENTFD2, [1, 0, 0, 0, 0, 0]);
    // struct pollfd { int fd; short events; short revents; }
    let mut fds = (efd as i32).to_le_bytes().to_vec();
    fds.extend_from_slice(&1i16.to_le_bytes()); // POLLIN
    fds.extend_from_slice(&0i16.to_le_bytes());
    p.mem.write(s, &fds).unwrap();
    assert_eq!(p.syscall(&mut t, nr::PPOLL, [s, 1, 0, 0, 8, 0]), 1);
    assert_eq!(i16::from_le_bytes(p.mem.read(s + 6, 2).unwrap().try_into().unwrap()) & 1, 1);
}
