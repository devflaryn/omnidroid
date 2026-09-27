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

/// This kernel is one without the tagged address ABI (`CONFIG_ARM64_TAGGED_ADDR_ABI` off): the
/// prctls answer `EINVAL`, so bionic keeps an untagged heap. A tagged heap pointer would reach the
/// host's GPU driver inside Vulkan structs, which no host can untag (D3a). A tagged pointer given
/// to a syscall still works (`guest::untag`), as TBI still holds in the CPU.
#[test]
fn the_tagged_address_abi_is_not_offered() {
    const EINVAL: u64 = -22i64 as u64;
    let (p, mut t, _) = process();
    assert_eq!(p.syscall(&mut t, nr::PRCTL, [55, 1, 0, 0, 0, 0]), EINVAL, "PR_SET_TAGGED_ADDR_CTRL(ENABLE)");
    assert_eq!(p.syscall(&mut t, nr::PRCTL, [56, 0, 0, 0, 0, 0]), EINVAL, "PR_GET_TAGGED_ADDR_CTRL");
}

/// A terminating signal with its default action is left pending for the run loop, which takes
/// the action on delivery (the A5 gate's `faults` fixture and bionic's abort show the process end).
#[test]
fn a_signal_with_its_default_action_is_pending_for_delivery() {
    let (p, mut t, _) = process();
    let pid = p.syscall(&mut t, nr::GETPID, [0; 6]);
    assert_eq!(p.syscall(&mut t, nr::RT_TGSIGQUEUEINFO, [pid, pid, 6, 0, 0, 0]), 0);
    assert_eq!(t.exit, None);
    assert_ne!(t.pending.load(std::sync::atomic::Ordering::SeqCst) & (1 << 5), 0, "SIGABRT pending");
}

#[test]
fn a_signal_that_is_ignored_by_default_changes_nothing() {
    let (p, mut t, _) = process();
    let pid = p.syscall(&mut t, nr::GETPID, [0; 6]);
    assert_eq!(p.syscall(&mut t, nr::TGKILL, [pid, pid, 17, 0, 0, 0]), 0, "SIGCHLD");
    assert_eq!(t.exit, None);
}

#[test]
fn umask_answers_the_previous_mask_and_keeps_only_permission_bits() {
    let (p, mut t, _) = process();
    assert_eq!(p.syscall(&mut t, nr::UMASK, [0, 0, 0, 0, 0, 0]), 0o022, "the default");
    assert_eq!(p.syscall(&mut t, nr::UMASK, [0o7777, 0, 0, 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::UMASK, [0o022, 0, 0, 0, 0, 0]), 0o777);
}

#[test]
fn membarrier_offers_the_private_expedited_commands_a_jit_registers_for() {
    let (p, mut t, _) = process();
    let offered = p.syscall(&mut t, omni_linux::syscall::nr::MEMBARRIER, [0, 0, 0, 0, 0, 0]);
    assert_eq!(offered & (8 | 16), 8 | 16, "{offered:#x}");
    assert_eq!(p.syscall(&mut t, omni_linux::syscall::nr::MEMBARRIER, [16, 0, 0, 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, omni_linux::syscall::nr::MEMBARRIER, [8, 0, 0, 0, 0, 0]), 0);
}

/// `CLOCK_MONOTONIC` is one clock for every process, as the kernel's is: a vsync timestamp the
/// composer sends, a fence's signal time and a frame's deadline are compared across processes.
#[test]
fn every_process_reads_one_monotonic_clock() {
    let read = |p: &Arc<Process>, t: &mut omni_linux::Task, s: u64| {
        assert_eq!(p.syscall(t, nr::CLOCK_GETTIME, [1, s, 0, 0, 0, 0]), 0);
        let b = p.mem.read(s, 16).unwrap();
        let secs = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let nanos = u64::from_le_bytes(b[8..16].try_into().unwrap());
        std::time::Duration::new(secs, nanos as u32)
    };
    let (a, mut ta, sa) = process();
    let first = read(&a, &mut ta, sa);
    std::thread::sleep(std::time::Duration::from_millis(60));
    let (b, mut tb, sb) = process();
    let later = read(&b, &mut tb, sb);
    assert!(later >= first + std::time::Duration::from_millis(50), "a process started later reads a later time: {first:?} then {later:?}");
}

/// And for every host process of an instance: one started later with the first one's origin
/// (`OMNI_MONOTONIC_ORIGIN`, as an app's host process is started) reads the same clock.
#[test]
fn another_host_process_given_the_origin_reads_the_same_clock() {
    if std::env::var("OMNI_CLOCK_CHILD").is_ok() {
        println!("monotonic-ns {}", omni_linux::sys::monotonic().as_nanos());
        return;
    }
    let origin = omni_linux::sys::monotonic_origin();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let before = omni_linux::sys::monotonic();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "another_host_process_given_the_origin_reads_the_same_clock", "--nocapture", "--test-threads=1"])
        .env("OMNI_CLOCK_CHILD", "1")
        .env("OMNI_MONOTONIC_ORIGIN", &origin)
        .output()
        .unwrap();
    let after = omni_linux::sys::monotonic();
    let text = String::from_utf8_lossy(&out.stdout);
    let child: u128 = text.lines().find_map(|l| l.split_once("monotonic-ns ").map(|(_, v)| v)).expect("the child's reading").trim().parse().unwrap();
    let slack = 5_000_000; // 5 ms: the wall clock's and the platform clock's readings at the anchor
    assert!(child + slack >= before.as_nanos() && child <= after.as_nanos() + slack, "child {child} outside {before:?}..{after:?}");
}

/// `capget` (crash_dump asks before dropping them): an app holds no capability; an unknown
/// header version is answered with version 3 and `EINVAL`.
#[test]
fn capget_reports_an_apps_empty_capability_sets() {
    let (p, mut t, s) = process();
    p.mem.write(s, &[0u8; 8]).unwrap();
    assert_eq!(p.syscall(&mut t, nr::CAPGET, [s, s + 64, 0, 0, 0, 0]), EINVAL.as_return());
    assert_eq!(p.mem.read_u32(s).unwrap(), 0x2008_0522);
    p.mem.write(s + 64, &[0xffu8; 24]).unwrap();
    assert_eq!(p.syscall(&mut t, nr::CAPGET, [s, s + 64, 0, 0, 0, 0]), 0);
    assert_eq!(p.mem.read(s + 64, 24).unwrap(), vec![0u8; 24]);
    assert_eq!(p.syscall(&mut t, nr::CAPSET, [s, s + 64, 0, 0, 0, 0]), 0);
}

/// The set*id calls: a user may keep its own ids (and `-1` changes nothing) but take no other;
/// `getresuid` and the group list answer what is held.
#[test]
fn an_app_keeps_its_ids_and_takes_no_others() {
    let (p, mut t, s) = process();
    let uid = p.syscall(&mut t, nr::GETUID, [0; 6]);
    assert_eq!(p.syscall(&mut t, nr::SETGID, [uid, 0, 0, 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::SETRESUID, [u64::from(u32::MAX), uid, u64::from(u32::MAX), 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::SETUID, [0, 0, 0, 0, 0, 0]), omni_linux::errno::EPERM.as_return());
    assert_eq!(p.syscall(&mut t, nr::SETGID, [1000, 0, 0, 0, 0, 0]), omni_linux::errno::EPERM.as_return());
    assert_eq!(p.syscall(&mut t, nr::GETRESUID, [s, s + 4, s + 8, 0, 0, 0]), 0);
    assert_eq!(p.mem.read(s, 12).unwrap(), [uid as u32; 3].iter().flat_map(|u| u.to_le_bytes()).collect::<Vec<_>>());
    assert_eq!(p.syscall(&mut t, nr::GETGROUPS, [0; 6]), 0);
    assert_eq!(p.syscall(&mut t, nr::SETGROUPS, [0, s, 0, 0, 0, 0]), omni_linux::errno::EPERM.as_return());
}

/// Capabilities: what the process was granted (system_server's `BLOCK_SUSPEND`, as the zygote
/// grants it) is what `capget` reports; `capset` drops but never takes; the bounding set holds
/// every one.
#[test]
fn capabilities_are_what_was_granted_and_can_only_be_dropped() {
    let (p, mut t, s) = process();
    let block_suspend = omni_linux::sys::cap_number("BLOCK_SUSPEND").unwrap();
    let wake_alarm = omni_linux::sys::cap_number("CAP_WAKE_ALARM").unwrap();
    assert_eq!((block_suspend, wake_alarm), (36, 35));
    p.sys.set_caps((1 << block_suspend) | (1 << wake_alarm));
    p.mem.write_u32(s, 0x2008_0522).unwrap();
    assert_eq!(p.syscall(&mut t, nr::CAPGET, [s, s + 64, 0, 0, 0, 0]), 0);
    let word = |n: u64| p.mem.read_u32(s + 64 + n * 4).unwrap();
    assert_eq!((word(0), word(3), word(4)), (0, 0b11 << 3, 0b11 << 3), "caps 35 and 36 live in the second word");
    // Dropping WAKE_ALARM is allowed; taking SYS_ADMIN is not.
    p.mem.write(s + 64, &[0u8; 24]).unwrap();
    p.mem.write_u32(s + 64 + 12, 1 << (block_suspend - 32)).unwrap();
    assert_eq!(p.syscall(&mut t, nr::CAPSET, [s, s + 64, 0, 0, 0, 0]), 0);
    assert_eq!(p.sys.caps(), 1 << block_suspend);
    p.mem.write_u32(s + 64, 1 << 21).unwrap();
    assert_eq!(p.syscall(&mut t, nr::CAPSET, [s, s + 64, 0, 0, 0, 0]), omni_linux::errno::EPERM.as_return());
    assert_eq!(p.syscall(&mut t, nr::PRCTL, [23, 21, 0, 0, 0, 0]), 1, "PR_CAPBSET_READ");
    assert_eq!(p.syscall(&mut t, nr::PRCTL, [47, 1, u64::from(block_suspend), 0, 0, 0]), 1, "PR_CAP_AMBIENT_IS_SET");
}
