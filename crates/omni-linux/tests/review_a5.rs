//! The A2-A5 review's findings, each reproduced before its fix.
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_linux::errno::{EINTR, EINVAL};
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, signal, vfs::{Sysroot, Vfs}, Task};

const SIGUSR1: u64 = 10;
const ETIMEDOUT: i64 = -110;

fn process() -> (Arc<Process>, u64) {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let s = p.scratch();
    (p, s)
}

fn pend(t: &Task, sig: u64) {
    t.pending.fetch_or(1 << (sig - 1), Ordering::SeqCst);
}

/// Critical 1: a signal posted before the task sleeps must still end the sleep.
#[test]
fn a_signal_pending_before_a_futex_wait_ends_it_at_once() {
    let (p, s) = process();
    p.mem.write_u32(s, 0).unwrap();
    let mut t = Task::new(2001, Arc::clone(&p));
    pend(&t, SIGUSR1);
    let start = Instant::now();
    assert_eq!(p.syscall(&mut t, nr::FUTEX, [s, 0, 0, 0, 0, 0]) as i64, -(EINTR.0 as i64));
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_blocked_pending_signal_does_not_end_a_futex_wait() {
    let (p, s) = process();
    p.mem.write_u32(s, 0).unwrap();
    p.mem.write(s + 64, &[0u8; 8]).unwrap();
    p.mem.write_u64(s + 72, 20_000_000).unwrap(); // 20 ms
    let mut t = Task::new(2002, Arc::clone(&p));
    t.sigmask = 1 << (SIGUSR1 - 1);
    pend(&t, SIGUSR1);
    assert_eq!(p.syscall(&mut t, nr::FUTEX, [s, 0, 0, s + 64, 0, 0]) as i64, ETIMEDOUT);
}

#[test]
fn a_signal_posted_to_a_task_blocked_in_futex_ends_the_wait() {
    let (p, s) = process();
    p.mem.write_u32(s, 0).unwrap();
    let t = Task::new(2003, Arc::clone(&p));
    let pending = Arc::clone(&t.pending);
    let waiter = {
        let p = Arc::clone(&p);
        let mut t = t;
        std::thread::spawn(move || p.syscall(&mut t, nr::FUTEX, [s, 0, 0, 0, 0, 0]) as i64)
    };
    while p.futexes.waiters(s) == 0 {
        std::thread::sleep(Duration::from_millis(2));
    }
    pending.fetch_or(1 << (SIGUSR1 - 1), Ordering::SeqCst);
    p.futexes.interrupt(2003);
    assert_eq!(waiter.join().unwrap(), -(EINTR.0 as i64));
}

/// Important 2: a sleep ends early for a signal.
#[test]
fn nanosleep_is_ended_by_a_signal() {
    let (p, s) = process();
    p.mem.write_u64(s, 10).unwrap(); // 10 s
    p.mem.write_u64(s + 8, 0).unwrap();
    let t = Task::new(2004, Arc::clone(&p));
    let pending = Arc::clone(&t.pending);
    let start = Instant::now();
    let sleeper = {
        let p = Arc::clone(&p);
        let mut t = t;
        std::thread::spawn(move || p.syscall(&mut t, nr::NANOSLEEP, [s, 0, 0, 0, 0, 0]) as i64)
    };
    std::thread::sleep(Duration::from_millis(50));
    pending.fetch_or(1 << (SIGUSR1 - 1), Ordering::SeqCst);
    p.futexes.interrupt(2004);
    assert_eq!(sleeper.join().unwrap(), -(EINTR.0 as i64));
    assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
}

/// Important 3a: timeouts from the guest never overflow the host's clock arithmetic.
#[test]
fn absurd_timeouts_are_errors_not_panics() {
    let (p, s) = process();
    p.mem.write_u32(s, 0).unwrap();
    let mut t = Task::new(2005, Arc::clone(&p));
    p.mem.write_u64(s + 64, (-1i64) as u64).unwrap(); // tv_sec = -1
    p.mem.write_u64(s + 72, 0).unwrap();
    assert_eq!(p.syscall(&mut t, nr::FUTEX, [s, 0, 0, s + 64, 0, 0]) as i64, -(EINVAL.0 as i64));
    assert_eq!(p.syscall(&mut t, nr::NANOSLEEP, [s + 64, 0, 0, 0, 0, 0]) as i64, -(EINVAL.0 as i64));
    // An absolute deadline at the end of time: no panic (the pending signal ends the wait).
    p.mem.write_u64(s + 64, i64::MAX as u64).unwrap();
    pend(&t, SIGUSR1);
    assert_eq!(p.syscall(&mut t, nr::FUTEX, [s, 9, 0, s + 64, 0, u32::MAX as u64]) as i64, -(EINTR.0 as i64));
    let mut t2 = Task::new(2006, Arc::clone(&p));
    p.mem.write_u64(s + 64, u64::MAX >> 1).unwrap();
    pend(&t2, SIGUSR1);
    assert_eq!(p.syscall(&mut t2, nr::FUTEX, [s, 0, 0, s + 64, 0, 0]) as i64, -(EINTR.0 as i64));
}

/// Important 3c: writev's total is capped as write's is.
#[test]
fn writev_of_many_huge_iovecs_is_capped_not_an_abort() {
    let (p, s) = process();
    let mut t = Task::new(2007, Arc::clone(&p));
    let big = p.syscall(&mut t, nr::MMAP, [0, 64 << 20, 3, 0x22, u64::MAX, 0]);
    assert!((big as i64) > 0);
    for i in 0..1024u64 {
        p.mem.write_u64(s + i * 16, big).unwrap();
        p.mem.write_u64(s + i * 16 + 8, 64 << 20).unwrap();
    }
    p.mem.write(s + 20000, b"/dev/null\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s + 20000, 1, 0, 0, 0]);
    let n = p.syscall(&mut t, nr::WRITEV, [fd, s, 1024, 0, 0, 0]) as i64;
    assert!(n > 0 && n <= 1 << 24, "{n}");
}

/// Important 3d: a guest-chosen alternate stack at the top of the address space does not overflow.
#[test]
fn an_alternate_stack_at_the_top_of_memory_does_not_panic() {
    let alt = signal::altstack(u64::MAX - 10, 100);
    let _ = signal::placement(0x1000, alt, true);
    let _ = signal::placement(u64::MAX - 5, alt, true);
}

/// Important 7: a syscall copying into guest memory while another thread unmaps it answers EFAULT
/// or succeeds -- never a host access violation.
#[test]
fn a_copy_racing_munmap_is_efault_not_a_host_crash() {
    let (p, s) = process();
    let page = p.mm.page_size();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut t = Task::new(2008, Arc::clone(&p));
    let at = p.syscall(&mut t, nr::MMAP, [0, page, 3, 0x22, u64::MAX, 0]);
    assert!((at as i64) > 0);
    let unmapper = {
        let (p, stop) = (Arc::clone(&p), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut t = Task::new(2009, Arc::clone(&p));
            while !stop.load(Ordering::Relaxed) {
                p.syscall(&mut t, nr::MUNMAP, [at, page, 0, 0, 0, 0]);
                p.syscall(&mut t, nr::MMAP, [at, page, 3, 0x32, u64::MAX, 0]); // MAP_FIXED
                p.syscall(&mut t, nr::MADVISE, [at, page, 4, 0, 0, 0]); // MADV_DONTNEED
            }
        })
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        // clock_gettime writes 16 bytes through the guest pointer.
        let r = p.syscall(&mut t, nr::CLOCK_GETTIME, [1, at + page - 16, 0, 0, 0, 0]) as i64;
        assert!(r == 0 || r == -14, "{r}");
        let _ = s;
    }
    stop.store(true, Ordering::Relaxed);
    unmapper.join().unwrap();
}

/// `sigwait` (ART's signal catcher): a pending signal in the set is taken and its number returned,
/// with its siginfo; one posted while waiting ends the wait; a timeout is EAGAIN.
#[test]
fn rt_sigtimedwait_takes_a_signal_from_the_set() {
    let (p, s) = process();
    let mut t = Task::new(2010, Arc::clone(&p));
    let set = 1u64 << (SIGUSR1 - 1);
    t.sigmask = set;
    p.mem.write_u64(s, set).unwrap();
    pend(&t, SIGUSR1);
    assert_eq!(p.syscall(&mut t, nr::RT_SIGTIMEDWAIT, [s, s + 64, 0, 8, 0, 0]), SIGUSR1);
    assert_eq!(p.mem.read(s + 64, 4).unwrap(), (SIGUSR1 as u32).to_le_bytes());
    assert_eq!(t.pending.load(Ordering::SeqCst) & set, 0, "taken, not left pending");
    // A 10 ms timeout with nothing pending.
    p.mem.write_u64(s + 256, 0).unwrap();
    p.mem.write_u64(s + 264, 10_000_000).unwrap();
    assert_eq!(p.syscall(&mut t, nr::RT_SIGTIMEDWAIT, [s, 0, s + 256, 8, 0, 0]) as i64, -11);
    // Posted while waiting.
    let pending = Arc::clone(&t.pending);
    let waiter = {
        let p = Arc::clone(&p);
        std::thread::spawn(move || p.syscall(&mut t, nr::RT_SIGTIMEDWAIT, [s, 0, 0, 8, 0, 0]))
    };
    std::thread::sleep(Duration::from_millis(30));
    pending.fetch_or(set, Ordering::SeqCst);
    p.futexes.interrupt(2010);
    assert_eq!(waiter.join().unwrap(), SIGUSR1);
}

/// `sigsuspend` waits with a temporary mask and answers EINTR once a signal is deliverable.
#[test]
fn rt_sigsuspend_waits_under_a_temporary_mask() {
    let (p, s) = process();
    let mut t = Task::new(2011, Arc::clone(&p));
    let usr1 = 1u64 << (SIGUSR1 - 1);
    t.sigmask = usr1;
    pend(&t, SIGUSR1);
    p.mem.write_u64(s, 0).unwrap(); // suspend with nothing blocked
    assert_eq!(p.syscall(&mut t, nr::RT_SIGSUSPEND, [s, 8, 0, 0, 0, 0]) as i64, -(EINTR.0 as i64));
    assert_eq!(t.sigmask, 0, "the temporary mask stands until the handler's frame records the old one");
    assert_eq!(t.saved_sigmask, Some(usr1));
}

/// A signal with its default action is acted on when it is *delivered*, to its target: a blocked
/// one stays pending (for `sigwait`), and the sender is never the one it kills. ART stops its
/// signal catcher with `tgkill(SIGQUIT)`, which that thread blocks and waits for.
#[test]
fn a_blocked_default_action_signal_stays_pending_and_does_not_kill_the_sender() {
    let (p, _s) = process();
    let mut t = Task::new(1000, Arc::clone(&p)); // the pid: the main task
    let quit = 1u64 << (3 - 1);
    t.sigmask = quit;
    assert_eq!(p.syscall(&mut t, nr::TGKILL, [1000, 1000, 3, 0, 0, 0]), 0);
    assert!(t.exit.is_none(), "the sender is not killed: {:?}", t.exit);
    assert_ne!(t.pending.load(Ordering::SeqCst) & quit, 0, "pending for sigwait");
}
