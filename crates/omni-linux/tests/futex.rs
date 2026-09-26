//! The futex wait queue (milestone A4): host threads act as guest tasks of one test process.
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_linux::errno::{EAGAIN, EINTR};
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}, Task};

const WAIT: u64 = 0;
const WAKE: u64 = 1;
const CMP_REQUEUE: u64 = 4;
const WAKE_OP: u64 = 5;
const WAIT_BITSET: u64 = 9;
const WAKE_BITSET: u64 = 10;
const PRIVATE: u64 = 128;
const ETIMEDOUT: i64 = -110;

fn process() -> (Arc<Process>, u64) {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let s = p.scratch();
    (p, s)
}

fn futex(p: &Arc<Process>, tid: i32, args: [u64; 6]) -> i64 {
    let mut t = Task::new(tid, Arc::clone(p));
    p.syscall(&mut t, nr::FUTEX, args) as i64
}

/// Start a waiter thread; it returns what its `futex` call answered.
fn waiter(p: &Arc<Process>, tid: i32, args: [u64; 6]) -> std::thread::JoinHandle<i64> {
    let p = Arc::clone(p);
    std::thread::spawn(move || futex(&p, tid, args))
}

fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn wake_answers_how_many_it_woke() {
    let (p, s) = process();
    p.mem.write_u32(s, 0).unwrap();
    let waiters: Vec<_> = (0..3).map(|i| waiter(&p, 2000 + i, [s, WAIT | PRIVATE, 0, 0, 0, 0])).collect();
    until("three waiters", || p.futexes.waiters(s) == 3);
    assert_eq!(futex(&p, 1, [s, WAKE | PRIVATE, 2, 0, 0, 0]), 2);
    assert_eq!(futex(&p, 1, [s, WAKE | PRIVATE, 10, 0, 0, 0]), 1);
    assert_eq!(futex(&p, 1, [s, WAKE | PRIVATE, 10, 0, 0, 0]), 0, "nobody left");
    for w in waiters {
        assert_eq!(w.join().unwrap(), 0);
    }
}

#[test]
fn a_changed_value_is_eagain_before_sleeping() {
    let (p, s) = process();
    p.mem.write_u32(s, 1).unwrap();
    assert_eq!(futex(&p, 1, [s, WAIT, 0, 0, 0, 0]), -(EAGAIN.0 as i64));
}

#[test]
fn a_bitset_wake_wakes_only_matching_waiters() {
    let (p, s) = process();
    p.mem.write_u32(s, 0).unwrap();
    let one = waiter(&p, 2001, [s, WAIT_BITSET, 0, 0, 0, 0b01]);
    let two = waiter(&p, 2002, [s, WAIT_BITSET, 0, 0, 0, 0b10]);
    until("two waiters", || p.futexes.waiters(s) == 2);
    assert_eq!(futex(&p, 1, [s, WAKE_BITSET, 10, 0, 0, 0b10]), 1);
    assert_eq!(two.join().unwrap(), 0);
    assert_eq!(p.futexes.waiters(s), 1, "the other bitset still waits");
    assert_eq!(futex(&p, 1, [s, WAKE, 10, 0, 0, 0]), 1);
    assert_eq!(one.join().unwrap(), 0);
}

#[test]
fn wait_bitset_takes_an_absolute_deadline() {
    let (p, s) = process();
    p.mem.write_u32(s, 0).unwrap();
    // CLOCK_MONOTONIC now, plus 30 ms, as bionic computes it.
    let mut t = Task::new(1, Arc::clone(&p));
    assert_eq!(p.syscall(&mut t, nr::CLOCK_GETTIME, [1, s + 64, 0, 0, 0, 0]), 0);
    let (sec, nsec) = (p.mem.read_u64(s + 64).unwrap(), p.mem.read_u64(s + 72).unwrap());
    let total = nsec + 30_000_000;
    p.mem.write_u64(s + 64, sec + total / 1_000_000_000).unwrap();
    p.mem.write_u64(s + 72, total % 1_000_000_000).unwrap();
    let start = Instant::now();
    assert_eq!(futex(&p, 1, [s, WAIT_BITSET, 0, s + 64, 0, u32::MAX as u64]), ETIMEDOUT);
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(20) && took < Duration::from_secs(2), "{took:?}");
}

#[test]
fn cmp_requeue_moves_waiters_and_checks_the_value() {
    let (p, s) = process();
    let (a, b) = (s, s + 8);
    p.mem.write_u32(a, 7).unwrap();
    let waiters: Vec<_> = (0..3).map(|i| waiter(&p, 2010 + i, [a, WAIT, 7, 0, 0, 0])).collect();
    until("three waiters on a", || p.futexes.waiters(a) == 3);
    assert_eq!(futex(&p, 1, [a, CMP_REQUEUE, 1, 2, b, 8]), -(EAGAIN.0 as i64), "a is 7, not 8");
    assert_eq!(futex(&p, 1, [a, CMP_REQUEUE, 1, 2, b, 7]), 3, "one woken, two requeued");
    until("two waiters on b", || p.futexes.waiters(b) == 2);
    assert_eq!(p.futexes.waiters(a), 0);
    assert_eq!(futex(&p, 1, [b, WAKE, 10, 0, 0, 0]), 2);
    for w in waiters {
        assert_eq!(w.join().unwrap(), 0);
    }
}

#[test]
fn wake_op_changes_the_second_word_and_wakes_by_the_comparison() {
    let (p, s) = process();
    let (a, b) = (s, s + 8);
    p.mem.write_u32(a, 0).unwrap();
    p.mem.write_u32(b, 5).unwrap();
    let on_b = waiter(&p, 2020, [b, WAIT, 5, 0, 0, 0]);
    until("a waiter on b", || p.futexes.waiters(b) == 1);
    // op = SET (0) with oparg 7; cmp = EQ (0) with cmparg 5.
    let op = (0u64 << 28) | (0 << 24) | (7 << 12) | 5;
    assert_eq!(futex(&p, 1, [a, WAKE_OP, 1, 1, b, op]), 1, "nobody on a, one on b");
    assert_eq!(p.mem.read(b, 4).unwrap(), 7u32.to_le_bytes(), "b was set to 7");
    assert_eq!(on_b.join().unwrap(), 0);
}

#[test]
fn a_tagged_address_is_the_same_futex() {
    let (p, s) = process();
    p.mem.write_u32(s, 0).unwrap();
    let tagged = s | (0xb4 << 56);
    let w = waiter(&p, 2030, [tagged, WAIT, 0, 0, 0, 0]);
    until("the tagged waiter", || p.futexes.waiters(s) == 1);
    assert_eq!(futex(&p, 1, [s, WAKE, 1, 0, 0, 0]), 1);
    assert_eq!(w.join().unwrap(), 0);
}

#[test]
fn an_interrupt_ends_a_wait_with_eintr() {
    let (p, s) = process();
    p.mem.write_u32(s, 0).unwrap();
    let w = waiter(&p, 2040, [s, WAIT, 0, 0, 0, 0]);
    until("a waiter", || p.futexes.waiters(s) == 1);
    p.futexes.interrupt_all();
    assert_eq!(w.join().unwrap(), -(EINTR.0 as i64));
}
