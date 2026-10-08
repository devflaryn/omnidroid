//! `poll_keyed` (`omni_linux::poll::KEYED`): a thread waiting in `epoll_pwait` on a netlink socket
//! (netd's, vold's, healthd's, ueventd's listeners) is woken by every change in
//! its host process while off -- here 8 such threads and 1000 changes told for something else --
//! and by none of them while on; and it still wakes for what it waits on: an answer queued on its
//! netlink socket, an eventfd added to its set while it waits (`epoll_ctl`) and then written.
//! One test per file: the wait queues are per host process.
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}, Task};

const AF_NETLINK: u64 = 16;
const SOCK_RAW: u64 = 3;
const EPOLL_CTL_ADD: u64 = 1;
const EPOLLIN: u32 = 1;

fn process() -> Arc<Process> {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    Process::for_tests(vfs, Output::Capture(Default::default()))
}

fn add(p: &Process, t: &mut Task, ep: u64, fd: u64, data: u64) {
    let ev = p.scratch() + 0x2000 + data * 16;
    let mut e = EPOLLIN.to_le_bytes().to_vec();
    e.extend_from_slice(&[0; 4]);
    e.extend_from_slice(&data.to_le_bytes());
    p.mem.write(ev, &e).unwrap();
    assert_eq!(p.syscall(t, nr::EPOLL_CTL, [ep, EPOLL_CTL_ADD, fd, ev, 0, 0]), 0);
}

/// One wait of `t` on `ep` (up to `ms`): the data of what was ready, or none.
fn wait(p: &Process, t: &mut Task, ep: u64, out: u64, ms: u64) -> Vec<u64> {
    let n = p.syscall(t, nr::EPOLL_PWAIT, [ep, out, 8, ms, 0, 0]) as i64;
    assert!(n >= 0, "epoll_pwait: {n}");
    (0..n as u64).map(|i| p.mem.read_u64(out + i * 16 + 8).unwrap()).collect()
}

#[test]
fn keyed_waits_are_not_woken_by_others_changes_and_still_see_their_own() {
    let p = process();
    let mut t = p.test_task();
    let s = p.scratch();
    let sock = p.syscall(&mut t, nr::SOCKET, [AF_NETLINK, SOCK_RAW, 0, 0, 0, 0]);
    assert!((sock as i64) >= 0, "socket: {}", sock as i64);

    // Eight waiters, each on an epoll set of its own holding the netlink socket.
    let mut report = Vec::new();
    for keyed in [false, true] {
        omni_linux::lever::apply(&format!("poll_keyed={}", u8::from(keyed))).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut threads = Vec::new();
        for i in 0..8 {
            let (p, stop) = (Arc::clone(&p), Arc::clone(&stop));
            let ep = p.syscall(&mut t, nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]);
            add(&p, &mut t, ep, sock, 1);
            threads.push(std::thread::spawn(move || {
                let mut t = Task::new(1000 + i + if keyed { 100 } else { 0 }, Arc::clone(&p));
                let out = p.scratch() + 0x4000 + u64::from(i as u32) * 0x200;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = wait(&p, &mut t, ep, out, 200);
                }
            }));
        }
        std::thread::sleep(Duration::from_millis(300));
        let (_, wakes_before) = omni_linux::poll::counts();
        let cpu_before = omni_platform::process::cpu_time().unwrap();
        let started = Instant::now();
        for i in 0..1000usize {
            // Something else changed (another process's binder work, a Looper's eventfd...), about
            // every 0.5-1 ms (a sleep: the notifier's own CPU stays out of the figure).
            omni_linux::poll::notify_key(0x5000_0000 + i);
            std::thread::sleep(Duration::from_micros(300));
        }
        let told = started.elapsed();
        std::thread::sleep(Duration::from_millis(50));
        let (_, wakes_after) = omni_linux::poll::counts();
        let cpu = omni_platform::process::cpu_time().unwrap() - cpu_before;
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        for th in threads {
            th.join().unwrap();
        }
        report.push((keyed, wakes_after - wakes_before, told, cpu));
    }
    for (keyed, wakes, told, cpu) in &report {
        eprintln!("[poll] poll_keyed={}: 8 netlink waiters, 1000 changes elsewhere in {told:?}: {wakes} wake-ups, process CPU {cpu:?}", u8::from(*keyed));
    }
    let (extra_wakes, extra_cpu) = (report[0].1.saturating_sub(report[1].1), report[0].3.saturating_sub(report[1].3));
    eprintln!("[poll] a spurious wake-up costs ~{:?} of CPU ({extra_wakes} more, {extra_cpu:?} more)", extra_cpu / u32::try_from(extra_wakes.max(1)).unwrap_or(u32::MAX));
    let (off, on) = (report[0].1, report[1].1);
    assert!(off > 2000, "off: every change wakes the waiters on anything ({off})");
    assert!(on < 400, "on: only their own slices and timeouts wake them ({on})");

    // On, a wait still sees its own changes.
    let mut t2 = Task::new(2000, Arc::clone(&p));
    let ep = p.syscall(&mut t2, nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]);
    add(&p, &mut t2, ep, sock, 1);
    let efd = p.syscall(&mut t2, nr::EVENTFD2, [0, 0, 0, 0, 0, 0]);
    let waiter = {
        let p = Arc::clone(&p);
        std::thread::spawn(move || {
            let mut t = Task::new(2001, Arc::clone(&p));
            let out = p.scratch() + 0x6000;
            let started = Instant::now();
            let got = wait(&p, &mut t, ep, out, 5000);
            (got, started.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    // An eventfd added to the set while it waits, then written: the wait must see the new member.
    add(&p, &mut t2, ep, efd, 2);
    std::thread::sleep(Duration::from_millis(20));
    p.mem.write(s + 0x100, &1u64.to_le_bytes()).unwrap();
    assert_eq!(p.syscall(&mut t2, nr::WRITE, [efd, s + 0x100, 8, 0, 0, 0]), 8);
    let (got, took) = waiter.join().unwrap();
    assert_eq!(got, vec![2], "the eventfd added while waiting");
    assert!(took < Duration::from_millis(2000), "woken by its own change, not a timeout ({took:?})");

    // An answer queued on the netlink socket (a dump request) wakes a waiter on it.
    let waiter = {
        let p = Arc::clone(&p);
        let ep2 = p.syscall(&mut t2, nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]);
        add(&p, &mut t2, ep2, sock, 1);
        std::thread::spawn(move || {
            let mut t = Task::new(2002, Arc::clone(&p));
            let started = Instant::now();
            let got = wait(&p, &mut t, ep2, p.scratch() + 0x7000, 5000);
            (got, started.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    // nlmsghdr: len 16+4, RTM_GETLINK (18), NLM_F_REQUEST|NLM_F_DUMP, seq 1, pid 0; rtgenmsg.
    let mut req = 20u32.to_le_bytes().to_vec();
    req.extend_from_slice(&18u16.to_le_bytes());
    req.extend_from_slice(&(1u16 | 0x300).to_le_bytes());
    req.extend_from_slice(&1u32.to_le_bytes());
    req.extend_from_slice(&0u32.to_le_bytes());
    req.extend_from_slice(&[0; 4]);
    p.mem.write(s + 0x200, &req).unwrap();
    assert_eq!(p.syscall(&mut t2, nr::SENDTO, [sock, s + 0x200, 20, 0, 0, 0]), 20);
    let (got, took) = waiter.join().unwrap();
    assert_eq!(got, vec![1], "the netlink socket's answer");
    assert!(took < Duration::from_millis(2000), "woken by its own socket, not a timeout ({took:?})");
    omni_linux::lever::apply("poll_keyed=0").unwrap();
}
