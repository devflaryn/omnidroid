//! What the busy system calls of a game world cost on the host's side, measured: each called N
//! times through the syscall table the way guest code makes it (its arguments in guest memory), on
//! descriptors that answer at once -- the kernel's own cost per call, which is what a guest thread
//! pays besides the work. `cargo test --release -p omni-linux --test syscall_cost -- --nocapture`.
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};
use std::time::Instant;

const N: u32 = 200_000;

#[test]
fn the_busy_system_calls_cost_little() {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    // A thread's stack is a lazy mapping partly committed: so is this.
    p.mem.write(s, &vec![0u8; 64 * 1024]).unwrap();
    let w64 = |at: u64, v: u64| p.mem.write(at, &v.to_le_bytes()).unwrap();
    let w32 = |at: u64, v: u32| p.mem.write(at, &v.to_le_bytes()).unwrap();
    let mut call = |number: u64, args: [u64; 6]| p.syscall(&mut t, number, args) as i64;
    let mut rows = Vec::new();
    let mut time = |name: &str, f: &mut dyn FnMut() -> i64, want: i64| {
        assert_eq!(f(), want, "{name}");
        let t0 = Instant::now();
        for _ in 0..N {
            std::hint::black_box(f());
        }
        let ns = t0.elapsed().as_nanos() as f64 / f64::from(N);
        rows.push(format!("{name} {ns:.1}"));
    };

    // futex: a FUTEX_WAIT whose word has changed (EAGAIN: reads the word), a FUTEX_WAKE of none.
    let word = s + 0x100;
    w32(word, 1);
    let mut c = |n, a| call(n, a);
    time("futex_wait_eagain", &mut || c(nr::FUTEX, [word, 128, 0, 0, 0, 0]), -11);
    time("futex_wake_none", &mut || c(nr::FUTEX, [word, 129, 1, 0, 0, 0]), 0);

    // rt_sigprocmask(SIG_SETMASK, &set, &old, 8).
    w64(s + 0x200, 0);
    time("rt_sigprocmask", &mut || c(nr::RT_SIGPROCMASK, [2, s + 0x200, s + 0x208, 8, 0, 0]), 0);

    // clock_gettime(CLOCK_MONOTONIC, &ts): what the vDSO answers without a call, here as one.
    time("clock_gettime", &mut || c(nr::CLOCK_GETTIME, [1, s + 0x210, 0, 0, 0, 0]), 0);

    // A pipe: write 8 bytes, read them back.
    assert_eq!(c(nr::PIPE2, [s + 0x300, 0, 0, 0, 0, 0]), 0);
    let fds = p.mem.read(s + 0x300, 8).unwrap();
    let (rd, wr) = (u64::from(u32::from_le_bytes(fds[0..4].try_into().unwrap())), u64::from(u32::from_le_bytes(fds[4..8].try_into().unwrap())));
    w64(s + 0x310, 0x0102_0304_0506_0708);
    time("pipe_write8+read8", &mut || c(nr::WRITE, [wr, s + 0x310, 8, 0, 0, 0]) + c(nr::READ, [rd, s + 0x320, 8, 0, 0, 0]), 16);

    // An eventfd, as a looper's wakeup: write 1, read the count.
    let efd = c(nr::EVENTFD2, [0, 0, 0, 0, 0, 0]) as u64;
    w64(s + 0x330, 1);
    time("eventfd_write+read", &mut || c(nr::WRITE, [efd, s + 0x330, 8, 0, 0, 0]) + c(nr::READ, [efd, s + 0x338, 8, 0, 0, 0]), 16);

    // epoll_pwait with one ready descriptor (an eventfd that stays readable), timeout 0.
    let epfd = c(nr::EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]) as u64;
    let ready = c(nr::EVENTFD2, [1, 0, 0, 0, 0, 0]) as u64;
    w32(s + 0x400, 1); // EPOLLIN
    w64(s + 0x408, 7);
    assert_eq!(c(nr::EPOLL_CTL, [epfd, 1, ready, s + 0x400, 0, 0]), 0);
    time("epoll_pwait_1ready", &mut || c(nr::EPOLL_PWAIT, [epfd, s + 0x420, 8, 0, 0, 8]), 1);

    // ppoll on that descriptor, timeout {0, 0}.
    w32(s + 0x500, ready as u32);
    w32(s + 0x504, 1); // POLLIN
    w64(s + 0x510, 0);
    w64(s + 0x518, 0);
    time("ppoll_1ready", &mut || c(nr::PPOLL, [s + 0x500, 1, s + 0x510, 0, 8, 0]), 1);

    // A unix socket pair: sendmsg 8 bytes in one iovec, recvmsg them.
    assert_eq!(c(nr::SOCKETPAIR, [1, 1, 0, s + 0x600, 0, 0]), 0);
    let sv = p.mem.read(s + 0x600, 8).unwrap();
    let (a, b) = (u64::from(u32::from_le_bytes(sv[0..4].try_into().unwrap())), u64::from(u32::from_le_bytes(sv[4..8].try_into().unwrap())));
    let msghdr = |at: u64, iov: u64| {
        w64(at, 0);
        w64(at + 8, 0);
        w64(at + 16, iov);
        w64(at + 24, 1);
        w64(at + 32, 0);
        w64(at + 40, 0);
        w64(at + 48, 0);
    };
    w64(s + 0x700, s + 0x310);
    w64(s + 0x708, 8);
    msghdr(s + 0x720, s + 0x700);
    w64(s + 0x760, s + 0x340);
    w64(s + 0x768, 8);
    msghdr(s + 0x780, s + 0x760);
    time("sendmsg8+recvmsg8", &mut || c(nr::SENDMSG, [a, s + 0x720, 0, 0, 0, 0]) + c(nr::RECVMSG, [b, s + 0x780, 0, 0, 0, 0]), 16);

    // One checked 4-byte read, through the API the kernel's handlers use.
    time("mem.read_u32", &mut || i64::from(p.mem.read_u32(word).unwrap()), 1);

    eprintln!("[syscall-cost] ns/call (x{N}): {}", rows.join(", "));
}
