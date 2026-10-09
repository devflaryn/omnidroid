//! `epoll_et=0`: the old behaviour, `EPOLLET` taken as level-triggered -- the spin `tests/poll.rs`'
//! edge-triggered tests guard against, reproduced: an eventfd written once and never read (mio's
//! waker) answers every `epoll_pwait(0)`. One test per file: the lever is process-wide.
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

#[test]
fn with_epoll_et_off_an_unread_eventfd_answers_every_wait() {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    let ep = p.syscall(&mut t, nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]);
    let efd = p.syscall(&mut t, nr::EVENTFD2, [0, 0o4000, 0, 0, 0, 0]);
    let mut ev = (1u32 | (1 << 31)).to_le_bytes().to_vec(); // EPOLLIN | EPOLLET
    ev.extend_from_slice(&[0; 4]);
    ev.extend_from_slice(&1u64.to_le_bytes());
    p.mem.write(s + 512, &ev).unwrap();
    assert_eq!(p.syscall(&mut t, nr::EPOLL_CTL, [ep, 1, efd, s + 512, 0, 0]), 0);
    p.mem.write_u64(s, 1).unwrap();
    assert_eq!(p.syscall(&mut t, nr::WRITE, [efd, s, 8, 0, 0, 0]), 8);
    let looks = |p: &Process, t: &mut omni_linux::Task| (0..100).filter(|_| p.syscall(t, nr::EPOLL_PWAIT, [ep, s + 1024, 8, 0, 0, 8]) == 1).count();
    assert_eq!(looks(&p, &mut t), 1, "edge-triggered (the default): one report");
    omni_linux::lever::apply("epoll_et=0").unwrap();
    assert_eq!(looks(&p, &mut t), 100, "level-triggered: every look -- tokio's driver never sleeps");
    omni_linux::lever::apply("epoll_et=1").unwrap();
}
