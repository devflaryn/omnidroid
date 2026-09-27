//! A transaction that cannot be placed in its receiver (a descriptor it carries cannot be
//! installed, its buffer cannot be written) strands nothing: a sync call's sender reads
//! BR_FAILED_REPLY instead of waiting forever, and a oneway call is dropped with its node's next
//! oneway let go -- the kernel fails such a transaction towards its sender, never the receiver.
//! Before, the receiver's read failed, the sender waited on, and the node's later oneways queued
//! behind one that never came back. One test per file: the broker is per host process.
mod binder_guest;

use binder_guest::*;
use omni_linux::syscall::nr;

#[test]
fn a_transaction_that_cannot_be_placed_fails_towards_its_sender() {
    let mut receiver = Guest::new();
    let mut r = receiver.looper();
    receiver.set_context_mgr(&mut r);
    let mut sender = Guest::new();
    let mut s = sender.thread();

    // The receive area goes away: nothing can be written into it.
    assert_eq!(receiver.p.syscall(&mut r, nr::MUNMAP, [receiver.area, AREA, 0, 0, 0, 0]), 0);

    let call = sender.transaction(BC_TRANSACTION, 0, 5, 0, b"sync", &[]);
    assert_eq!(cmds(&sender.read(&mut s, &call)), [BR_TRANSACTION_COMPLETE]);
    let (res, got) = receiver.write_read(&mut r, &[], true);
    assert!(codes(&got).is_empty(), "nothing reaches the receiver: {got:x?}");
    assert!(res == 0 || res == -11, "the receiver's read is not failed by it: {res}");
    assert_eq!(cmds(&sender.read(&mut s, &[])), [BR_FAILED_REPLY], "the caller hears its call failed");

    let first = sender.transaction(BC_TRANSACTION, 0, 1, TF_ONE_WAY, b"one", &[]);
    assert_eq!(cmds(&sender.read(&mut s, &first)), [BR_TRANSACTION_COMPLETE]);
    assert!(codes(&receiver.read(&mut r, &[])).is_empty());

    // The area is back: the node's next oneway is delivered, not queued behind the lost one.
    let at = receiver.p.syscall(&mut r, nr::MMAP, [receiver.area, AREA, 3, 0x32, u64::MAX, 0]);
    assert_eq!(at, receiver.area);
    let second = sender.transaction(BC_TRANSACTION, 0, 2, TF_ONE_WAY, b"two", &[]);
    assert_eq!(cmds(&sender.read(&mut s, &second)), [BR_TRANSACTION_COMPLETE]);
    let got = receiver.read(&mut r, &[]);
    assert_eq!(codes(&got), [2], "the next oneway goes");
    assert_eq!(got[0].data, b"two");
}
