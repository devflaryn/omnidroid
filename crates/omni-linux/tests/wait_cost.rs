//! What a guest thread's sleep costs the host, measured: a wait that times out (a looper's
//! `epoll_pwait`, an HttpClient's `ppoll`, a `futex` with a timeout) and a herd -- every waiter of
//! the host process woken by `wake_everyone` (a posted signal, a process's end) to look and sleep
//! again. Each arm under the `waiter_park` and `wake_dedupe` levers, phase against phase.
//! `cargo test --release -p omni-linux --test wait_cost -- --nocapture`.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}, Task};

fn process() -> Arc<Process> {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    Process::for_tests(vfs, Output::Capture(Default::default()))
}

/// This thread's processor time.
#[cfg(unix)]
fn thread_cpu() -> Duration {
    unsafe extern "C" {
        fn clock_gettime(clock: i32, ts: *mut [i64; 2]) -> i32;
    }
    const CLOCK_THREAD_CPUTIME_ID: i32 = if cfg!(target_os = "macos") { 16 } else { 3 };
    let mut ts = [0i64; 2];
    // SAFETY: a valid clock id and a two-word timespec.
    assert_eq!(unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &raw mut ts) }, 0);
    Duration::new(ts[0] as u64, ts[1] as u32)
}

/// The levers are the host process's: one measurement at a time.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Set a lever; whether this build has it.
fn lever(line: &str) -> bool {
    omni_linux::lever::apply(line).is_some()
}

/// Waits that time out after 1 ms, nobody waking them: processor time and wall time per wait.
#[cfg(unix)]
#[test]
fn a_sleeping_wait_costs_little() {
    const N: u32 = 400;
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let p = process();
    let mut t = p.test_task();
    let s = p.scratch();
    let mut call = |number: u64, args: [u64; 6]| p.syscall(&mut t, number, args) as i64;
    let efd = call(nr::EVENTFD2, [0, 0, 0, 0, 0, 0]) as u64;
    let ep = call(nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]) as u64;
    p.mem.write(s + 0x400, &1u32.to_le_bytes()).unwrap(); // EPOLLIN
    p.mem.write(s + 0x408, &7u64.to_le_bytes()).unwrap();
    assert_eq!(call(nr::EPOLL_CTL, [ep, 1, efd, s + 0x400, 0, 0]), 0);
    // ppoll's pollfd and a 1 ms timespec; the futex word; nanosleep's 1 ms.
    p.mem.write(s + 0x500, &(efd as u32).to_le_bytes()).unwrap();
    p.mem.write(s + 0x504, &1u32.to_le_bytes()).unwrap();
    p.mem.write(s + 0x510, &0u64.to_le_bytes()).unwrap();
    p.mem.write(s + 0x518, &1_000_000u64.to_le_bytes()).unwrap();
    p.mem.write(s + 0x600, &0u32.to_le_bytes()).unwrap();
    let mut rows = Vec::new();
    let mut time = |name: &str, f: &mut dyn FnMut() -> i64, want: i64| {
        assert_eq!(f(), want, "{name}");
        let (c0, w0) = (thread_cpu(), Instant::now());
        for _ in 0..N {
            std::hint::black_box(f());
        }
        let (cpu, wall) = (thread_cpu() - c0, w0.elapsed());
        rows.push(format!("{name:<34} cpu {:6.2} us/wait  wall {:6.3} ms/wait", cpu.as_nanos() as f64 / 1e3 / f64::from(N), wall.as_nanos() as f64 / 1e6 / f64::from(N)));
    };
    for park in ["0", "1", "0", "1"] {
        let known = lever(&format!("waiter_park={park}"));
        let arm = if known { format!("waiter_park={park}") } else { "(no lever)".into() };
        time(&format!("ppoll 1 fd 1 ms {arm}"), &mut || call(nr::PPOLL, [s + 0x500, 1, s + 0x510, 0, 8, 0]), 0);
        time(&format!("epoll_pwait 1 fd 1 ms {arm}"), &mut || call(nr::EPOLL_PWAIT, [ep, s + 0x420, 8, 1, 0, 8]), 0);
    }
    // futex (private, relative 1 ms) and nanosleep: the futex queue's wait, no lever.
    time("futex_wait 1 ms", &mut || call(nr::FUTEX, [s + 0x600, 128, 0, s + 0x510, 0, 0]), -110);
    time("nanosleep 1 ms", &mut || call(nr::NANOSLEEP, [s + 0x510, 0, 0, 0, 0, 0]), 0);
    // The same futex wait while 256 other tasks wait on words of their own (a game's threads):
    // its timeout takes it out of the queues.
    let others: Vec<_> = (0..256u64)
        .map(|i| {
            let p = Arc::clone(&p);
            std::thread::spawn(move || {
                let mut t = Task::new(6000 + i as i32, Arc::clone(&p));
                p.syscall(&mut t, nr::FUTEX, [s + 0x2000 + i * 4, 128, 0, 0, 0, 0])
            })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    while (0..256u64).map(|i| p.futexes.waiters(s + 0x2000 + i * 4)).sum::<usize>() < 256 {
        assert!(Instant::now() < deadline, "the other waiters");
        std::thread::sleep(Duration::from_millis(5));
    }
    time("futex_wait 1 ms beside 256 waits", &mut || call(nr::FUTEX, [s + 0x600, 128, 0, s + 0x510, 0, 0]), -110);
    for i in 0..256u64 {
        call(nr::FUTEX, [s + 0x2000 + i * 4, 129, 1, 0, 0, 0]);
    }
    for o in others {
        o.join().unwrap();
    }
    lever("waiter_park=1");
    eprintln!("[wait_cost] sleeping waits, {N} each:\n  {}", rows.join("\n  "));
}

/// `n` guest threads wait in `epoll_pwait` on `k` eventfds each (a looper's set), then `rounds`
/// herds wake them all, 20 ms apart: the host process's processor time per herd, less the idle
/// cost of as long without one, and the waits that ended per herd.
fn herd(n: usize, k: usize, rounds: u32) -> (f64, f64) {
    let p = process();
    let stop = Arc::new(AtomicBool::new(false));
    let mut stops = Vec::new();
    let mut threads = Vec::new();
    let (tx, rx) = std::sync::mpsc::channel();
    for i in 0..n {
        let (p, stop, tx) = (Arc::clone(&p), Arc::clone(&stop), tx.clone());
        threads.push(std::thread::spawn(move || {
            let mut t = Task::new(5000 + i as i32, Arc::clone(&p));
            let s = p.scratch() + 0x1000 + i as u64 * 64;
            let mut call = |number: u64, args: [u64; 6]| p.syscall(&mut t, number, args) as i64;
            let ep = call(nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]) as u64;
            let mut first = 0;
            for j in 0..k {
                let efd = call(nr::EVENTFD2, [0, 0, 0, 0, 0, 0]) as u64;
                p.mem.write(s, &1u32.to_le_bytes()).unwrap();
                p.mem.write(s + 8, &(j as u64).to_le_bytes()).unwrap();
                assert_eq!(call(nr::EPOLL_CTL, [ep, 1, efd, s, 0, 0]), 0);
                if j == 0 {
                    first = efd;
                }
            }
            tx.send(first).unwrap();
            while !stop.load(Ordering::SeqCst) {
                call(nr::EPOLL_PWAIT, [ep, s + 16, 2, u64::MAX, 0, 8]);
            }
        }));
    }
    for _ in 0..n {
        stops.push(rx.recv().unwrap());
    }
    std::thread::sleep(Duration::from_millis(300));
    let cpu = || omni_platform::process::cpu_time().unwrap();
    let (c0, w0) = (cpu(), omni_linux::poll::counts().1);
    for _ in 0..rounds {
        omni_linux::poll::wake_everyone();
        std::thread::sleep(Duration::from_millis(20));
    }
    let (c1, w1) = (cpu(), omni_linux::poll::counts().1);
    std::thread::sleep(Duration::from_millis(20 * u64::from(rounds)));
    let c2 = cpu();
    stop.store(true, Ordering::SeqCst);
    let mut t = p.test_task();
    let at = p.scratch() + 0x800;
    p.mem.write(at, &1u64.to_le_bytes()).unwrap();
    for fd in stops {
        p.syscall(&mut t, nr::WRITE, [fd, at, 8, 0, 0, 0]);
    }
    for th in threads {
        th.join().unwrap();
    }
    let per = |d: Duration| d.as_nanos() as f64 / 1e6 / f64::from(rounds);
    (per(c1 - c0) - per(c2 - c1), (w1 - w0) as f64 / f64::from(rounds))
}

#[test]
fn a_herd_costs_little() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (n, k, rounds) = (256, 6, 40);
    let mut rows = Vec::new();
    for (dedupe, park) in [("0", "0"), ("1", "0"), ("0", "1"), ("1", "1"), ("0", "0"), ("1", "1")] {
        let known = lever(&format!("wake_dedupe={dedupe}")) & lever(&format!("waiter_park={park}"));
        let arm = if known { format!("wake_dedupe={dedupe} waiter_park={park}") } else { "(no levers)".into() };
        let (ms, woken) = herd(n, k, rounds);
        rows.push(format!("{arm:<30} {ms:6.2} ms of CPU per herd, {woken:5.0} waits ended per herd"));
    }
    lever("wake_dedupe=1");
    lever("waiter_park=1");
    eprintln!("[wait_cost] herds of {n} waiters on {k} keys each, {rounds} per arm:\n  {}", rows.join("\n  "));
}
