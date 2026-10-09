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

const EPOLLET: u32 = 1 << 31;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLL_CTL_MOD: u64 = 3;

/// One `epoll_pwait` of up to `ms` (0: a look): how many events, and the first one's bits.
fn wait_once(p: &Process, t: &mut omni_linux::Task, s: u64, ep: u64, ms: u64) -> (u64, u32) {
    let n = p.syscall(t, nr::EPOLL_PWAIT, [ep, s + 1024, 8, ms, 0, 8]);
    let bits = if n > 0 { u32::from_le_bytes(p.mem.read(s + 1024, 4).unwrap().try_into().unwrap()) } else { 0 };
    (n, bits)
}

/// **`EPOLLET` is edge-triggered**: an eventfd written once and never read -- mio's waker, which
/// tokio's driver waits on -- is reported once, not on every wait. Treated as level-triggered it
/// answered every `epoll_pwait(0)` at once, and DnsResolver's `doh-handler` (tokio) spun at 99%
/// of a core (system host, 2026-10-09). Each later write is a new edge, though the descriptor
/// never stopped being readable.
#[test]
fn an_edge_triggered_eventfd_is_reported_once_per_write() {
    let (p, mut t, s) = process();
    let ep = p.syscall(&mut t, nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]);
    let efd = p.syscall(&mut t, nr::EVENTFD2, [0, EFD_NONBLOCK, 0, 0, 0, 0]);
    epoll_add(&p, &mut t, s, ep, efd, EPOLLIN | EPOLLET, 1);
    assert_eq!(wait_once(&p, &mut t, s, ep, 0).0, 0, "nothing written yet");
    assert_eq!(write_u64(&p, &mut t, s + 64, efd, 1), 8);
    assert_eq!(wait_once(&p, &mut t, s, ep, 0), (1, EPOLLIN));
    for i in 0..1000 {
        assert_eq!(wait_once(&p, &mut t, s, ep, 0).0, 0, "look {i}: still readable, but no new edge");
    }
    assert_eq!(write_u64(&p, &mut t, s + 64, efd, 1), 8);
    assert_eq!(wait_once(&p, &mut t, s, ep, 0), (1, EPOLLIN), "a second write while readable");
    assert_eq!(wait_once(&p, &mut t, s, ep, 0).0, 0);
    // A wait sleeps until the next write from another thread, then reports it once.
    let writer = {
        let p = Arc::clone(&p);
        let mut t2 = p.test_task();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            p.mem.write_u64(s + 2048, 1).unwrap();
            p.syscall(&mut t2, nr::WRITE, [efd, s + 2048, 8, 0, 0, 0])
        })
    };
    let started = Instant::now();
    assert_eq!(wait_once(&p, &mut t, s, ep, 5000).0, 1);
    assert!(started.elapsed() >= Duration::from_millis(30), "it slept until the write");
    assert_eq!(writer.join().unwrap(), 8);
    assert_eq!(wait_once(&p, &mut t, s, ep, 0).0, 0);
    // Level-triggered, the same eventfd is reported for as long as it is readable.
    let lt = p.syscall(&mut t, nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]);
    epoll_add(&p, &mut t, s, lt, efd, EPOLLIN, 2);
    assert_eq!(wait_once(&p, &mut t, s, lt, 0).0, 1);
    assert_eq!(wait_once(&p, &mut t, s, lt, 0).0, 1);
}

/// A descriptor that is always writable (a UDP socket is; a socket pair's end with room is) and
/// registered `EPOLLIN | EPOLLOUT | EPOLLET`, as mio registers every socket: writable is reported
/// once; data arriving is a new edge, reported with what is ready then.
#[test]
fn an_always_writable_socket_is_reported_once_per_change_when_edge_triggered() {
    let (p, mut t, s) = process();
    // AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK
    assert_eq!(p.syscall(&mut t, nr::SOCKETPAIR, [1, 2 | 0o4000, 0, s + 3000, 0, 0]), 0);
    let pair = p.mem.read(s + 3000, 8).unwrap();
    let a = u64::from(u32::from_le_bytes(pair[0..4].try_into().unwrap()));
    let b = u64::from(u32::from_le_bytes(pair[4..8].try_into().unwrap()));
    let ep = p.syscall(&mut t, nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]);
    epoll_add(&p, &mut t, s, ep, a, EPOLLIN | EPOLLOUT | EPOLLET, 1);
    assert_eq!(wait_once(&p, &mut t, s, ep, 0), (1, EPOLLOUT), "writable, once");
    for _ in 0..100 {
        assert_eq!(wait_once(&p, &mut t, s, ep, 0).0, 0, "no change, no event");
    }
    p.mem.write(s + 4000, b"datagram").unwrap();
    assert_eq!(p.syscall(&mut t, nr::WRITE, [b, s + 4000, 8, 0, 0, 0]), 8);
    assert_eq!(wait_once(&p, &mut t, s, ep, 0), (1, EPOLLIN | EPOLLOUT), "a datagram arrived");
    assert_eq!(wait_once(&p, &mut t, s, ep, 0).0, 0, "unread, but no new edge");
}

/// An edge-triggered timerfd is reported at each expiry, its count read in between -- the edge
/// of the next expiry is not lost to the read.
#[test]
fn an_edge_triggered_timerfd_is_reported_at_each_expiry() {
    let (p, mut t, s) = process();
    let fd = p.syscall(&mut t, nr::TIMERFD_CREATE, [CLOCK_MONOTONIC, EFD_NONBLOCK, 0, 0, 0, 0]);
    // itimerspec { interval 20 ms, value 20 ms }
    for (i, v) in [0u64, 20_000_000, 0, 20_000_000].iter().enumerate() {
        p.mem.write_u64(s + i as u64 * 8, *v).unwrap();
    }
    assert_eq!(p.syscall(&mut t, nr::TIMERFD_SETTIME, [fd, 0, s, 0, 0, 0]), 0);
    let ep = p.syscall(&mut t, nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]);
    epoll_add(&p, &mut t, s, ep, fd, EPOLLIN | EPOLLET, 1);
    for round in 0..3 {
        assert_eq!(wait_once(&p, &mut t, s, ep, 2000).0, 1, "expiry {round}");
        assert_eq!(read_u64(&p, &mut t, s + 64, fd).0, 8);
    }
    // Expired but not read: reported once, then not again (no read, no new edge).
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(wait_once(&p, &mut t, s, ep, 0).0, 1);
    assert_eq!(wait_once(&p, &mut t, s, ep, 0).0, 0);
}

/// `EPOLLONESHOT`: reported once, then nothing until `EPOLL_CTL_MOD` re-arms it.
#[test]
fn a_oneshot_entry_waits_for_its_rearm() {
    let (p, mut t, s) = process();
    let ep = p.syscall(&mut t, nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]);
    let efd = p.syscall(&mut t, nr::EVENTFD2, [1, EFD_NONBLOCK, 0, 0, 0, 0]);
    epoll_add(&p, &mut t, s, ep, efd, EPOLLIN | EPOLLONESHOT, 1);
    assert_eq!(wait_once(&p, &mut t, s, ep, 0).0, 1);
    assert_eq!(write_u64(&p, &mut t, s + 64, efd, 1), 8);
    assert_eq!(wait_once(&p, &mut t, s, ep, 0).0, 0, "disarmed");
    let mut ev = (EPOLLIN | EPOLLONESHOT).to_le_bytes().to_vec();
    ev.extend_from_slice(&[0; 4]);
    ev.extend_from_slice(&1u64.to_le_bytes());
    p.mem.write(s + 512, &ev).unwrap();
    assert_eq!(p.syscall(&mut t, nr::EPOLL_CTL, [ep, EPOLL_CTL_MOD, efd, s + 512, 0, 0]), 0);
    assert_eq!(wait_once(&p, &mut t, s, ep, 0).0, 1, "re-armed");
}
