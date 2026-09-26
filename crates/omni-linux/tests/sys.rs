use std::sync::Arc;

use omni_linux::errno::{EAGAIN, EINVAL};
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

fn process() -> (Arc<Process>, omni_linux::Task, u64) {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    let s = p.scratch();
    (p, t, s)
}

#[test]
fn uname_says_linux_aarch64() {
    let (p, mut t, s) = process();
    assert_eq!(p.syscall(&mut t, nr::UNAME, [s, 0, 0, 0, 0, 0]), 0);
    let u = p.mem.read(s, 6 * 65).unwrap();
    assert_eq!(&u[..6], b"Linux\0");
    assert_eq!(&u[4 * 65..4 * 65 + 8], b"aarch64\0");
}

#[test]
fn ids_are_stable_and_set_tid_address_answers_the_tid() {
    let (p, mut t, s) = process();
    let pid = p.syscall(&mut t, nr::GETPID, [0; 6]);
    assert_eq!(p.syscall(&mut t, nr::GETTID, [0; 6]), pid, "the main thread's tid is the pid");
    assert_eq!(p.syscall(&mut t, nr::SET_TID_ADDRESS, [s, 0, 0, 0, 0, 0]), pid);
    assert_eq!(t.clear_child_tid, s);
    assert!(p.syscall(&mut t, nr::GETUID, [0; 6]) >= 10000, "an app uid");
}

#[test]
fn clock_gettime_is_monotonic_and_getrandom_fills() {
    let (p, mut t, s) = process();
    assert_eq!(p.syscall(&mut t, nr::CLOCK_GETTIME, [1, s, 0, 0, 0, 0]), 0);
    let ts = |p: &Process| (p.mem.read_u64(s).unwrap(), p.mem.read_u64(s + 8).unwrap());
    let a = ts(&p);
    std::thread::sleep(std::time::Duration::from_millis(2));
    assert_eq!(p.syscall(&mut t, nr::CLOCK_GETTIME, [1, s, 0, 0, 0, 0]), 0);
    let b = ts(&p);
    assert!(b > a, "(sec, nsec) moves forward: {a:?} then {b:?}");
    assert!(b.1 < 1_000_000_000, "nanoseconds are normalized");
    assert_eq!(p.syscall(&mut t, nr::GETRANDOM, [s + 64, 32, 0, 0, 0, 0]), 32);
    assert_ne!(p.mem.read(s + 64, 32).unwrap(), [0; 32]);
}

#[test]
fn rt_sigaction_stores_and_returns_the_old_action_and_refuses_sigkill() {
    let (p, mut t, s) = process();
    p.mem.write(s, &[0x11; 32]).unwrap();
    assert_eq!(p.syscall(&mut t, nr::RT_SIGACTION, [11, s, 0, 8, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::RT_SIGACTION, [11, 0, s + 64, 8, 0, 0]), 0);
    assert_eq!(p.mem.read(s + 64, 32).unwrap(), [0x11; 32]);
    assert_eq!(p.syscall(&mut t, nr::RT_SIGACTION, [9, s, 0, 8, 0, 0]) as i64, -(EINVAL.0 as i64));
}

#[test]
fn a_futex_wait_on_a_changed_value_is_eagain_and_a_wake_with_no_waiters_is_zero() {
    let (p, mut t, s) = process();
    p.mem.write_u32(s, 1).unwrap();
    assert_eq!(p.syscall(&mut t, nr::FUTEX, [s, 128, 0, 0, 0, 0]) as i64, -(EAGAIN.0 as i64));
    assert_eq!(p.syscall(&mut t, nr::FUTEX, [s, 129, 1, 0, 0, 0]), 0);
}

#[test]
fn a_futex_wait_with_a_timeout_returns_etimedout() {
    let (p, mut t, s) = process();
    p.mem.write_u32(s, 0).unwrap();
    p.mem.write(s + 32, &[0u8; 8]).unwrap(); // tv_sec 0
    p.mem.write(s + 40, &5_000_000u64.to_le_bytes()).unwrap(); // 5 ms
    assert_eq!(p.syscall(&mut t, nr::FUTEX, [s, 128, 0, s + 32, 0, 0]) as i64, -110, "ETIMEDOUT");
}

#[test]
fn the_scheduler_is_sched_other_at_priority_zero() {
    let (p, mut t, s) = process();
    assert_eq!(p.syscall(&mut t, nr::SCHED_GETSCHEDULER, [0; 6]), 0, "SCHED_OTHER");
    p.mem.write_u32(s, 7).unwrap();
    assert_eq!(p.syscall(&mut t, nr::SCHED_GETPARAM, [0, s, 0, 0, 0, 0]), 0);
    assert_eq!(p.mem.read(s, 4).unwrap(), [0; 4], "sched_priority 0");
    assert_eq!(p.syscall(&mut t, nr::SCHED_GET_PRIORITY_MAX, [0; 6]), 0);
}

#[test]
fn the_tagged_address_abi_is_accepted_and_reported() {
    let (p, mut t, _) = process();
    assert_eq!(p.syscall(&mut t, nr::PRCTL, [56, 0, 0, 0, 0, 0]), 0, "PR_GET_TAGGED_ADDR_CTRL: off at first");
    assert_eq!(p.syscall(&mut t, nr::PRCTL, [55, 1, 0, 0, 0, 0]), 0, "PR_SET_TAGGED_ADDR_CTRL(ENABLE)");
    assert_eq!(p.syscall(&mut t, nr::PRCTL, [56, 0, 0, 0, 0, 0]), 1);
}

#[test]
fn a_signal_with_its_default_action_ends_the_process_by_that_signal() {
    let (p, mut t, _) = process();
    let pid = p.syscall(&mut t, nr::GETPID, [0; 6]);
    assert_eq!(p.syscall(&mut t, nr::RT_TGSIGQUEUEINFO, [pid, pid, 6, 0, 0, 0]), 0);
    assert_eq!(t.exit, Some(omni_linux::Exit::Signal(6)), "SIGABRT, default action: terminate");
}

#[test]
fn a_signal_that_is_ignored_by_default_changes_nothing() {
    let (p, mut t, _) = process();
    let pid = p.syscall(&mut t, nr::GETPID, [0; 6]);
    assert_eq!(p.syscall(&mut t, nr::TGKILL, [pid, pid, 17, 0, 0, 0]), 0, "SIGCHLD");
    assert_eq!(t.exit, None);
}
