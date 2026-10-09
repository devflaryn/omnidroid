//! A spawn request that never reached the process is not counted as made. A looper that takes the
//! process's work while no other looper idles is told `BR_SPAWN_LOOPER` -- once: until a thread
//! registers, no further one is asked for (`spawn_requested`). When the read that carried the
//! request fails instead (its buffer unwritable: EFAULT; a signal: EINTR), no thread will ever
//! register for it, and a count left behind froze the pool at its size for good -- a process whose
//! loopers then all block in calls that need a new incoming call hangs, where Linux (which decides
//! only once the read succeeds) asks again. One test per file: the broker is per host process.
mod binder_guest;

use binder_guest::*;
use omni_linux::syscall::nr;

const BINDER_SET_MAX_THREADS: u64 = 0x4004_6205;

#[test]
fn a_spawn_request_lost_with_its_read_is_asked_again() {
    let mut manager = Guest::new();
    let mut m = manager.looper();
    manager.set_context_mgr(&mut m);

    let mut receiver = Guest::new();
    let mut setup = receiver.thread();
    receiver.p.mem.write(receiver.s + 0x50, &15u32.to_le_bytes()).unwrap();
    assert_eq!(receiver.p.syscall(&mut setup, nr::IOCTL, [receiver.fd, BINDER_SET_MAX_THREADS, receiver.s + 0x50, 0, 0, 0]), 0);
    let mut waiting = receiver.looper();
    let mut l = receiver.looper();

    // A looper of the receiver hands the manager one of its objects in a call and waits for the
    // reply (so it is busy, never idle): the manager gets a handle to the object.
    let call = receiver.transaction(BC_TRANSACTION, 0, 9, 0, &object(TYPE_BINDER, 0x100, 0x200), &[0]);
    assert_eq!(receiver.write_read(&mut waiting, &call, false).0, 0);
    let _ = receiver.read(&mut waiting, &[]);
    let got = manager.read(&mut m, &[]);
    assert_eq!(codes(&got), [9]);
    let handle = u32::from_le_bytes(got[0].data[8..12].try_into().unwrap());

    // The manager calls the object (oneway): work for the receiver, which `l` (the only idle
    // looper) takes -- with a request to spawn -- into a read buffer it cannot write.
    let oneway = manager.transaction(BC_TRANSACTION, handle, 1, TF_ONE_WAY, b"x", &[]);
    assert_eq!(manager.write_read(&mut m, &oneway, false).0, 0);
    let bwr = receiver.s + 0x100;
    let mut b = Vec::new();
    for v in [0u64, 0, 0, 0x1000, 0, 0x10] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    receiver.p.mem.write(bwr, &b).unwrap();
    let r = receiver.p.syscall(&mut l, nr::IOCTL, [receiver.fd, BINDER_WRITE_READ, bwr, 0, 0, 0]) as i64;
    assert_eq!(r, -14, "the read into an unwritable buffer is EFAULT");

    // More work: `l` takes it with nothing else idle, so it is asked to spawn -- the first request
    // never arrived.
    let again = manager.transaction(BC_TRANSACTION, handle, 2, TF_ONE_WAY, b"y", &[]);
    assert_eq!(manager.write_read(&mut m, &again, false).0, 0);
    let mut seen = Vec::new();
    for _ in 0..4 {
        let got = receiver.read(&mut l, &[]);
        if got.is_empty() {
            break;
        }
        seen.extend(got);
    }
    assert!(!codes(&seen).is_empty(), "the receiver got its work: {:x?}", cmds(&seen));
    assert!(cmds(&seen).contains(&BR_SPAWN_LOOPER), "a looper is asked for again: {:x?}", cmds(&seen));
}
