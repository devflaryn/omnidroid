//! A process's work (a call to one of its objects that no thread of it is waiting in) goes only
//! to a thread available for it, as the kernel's `binder_available_for_proc_work_ilocked` says: a
//! looper (BC_ENTER_LOOPER / BC_REGISTER_LOOPER) with no transaction on its stack. A thread that
//! never entered the looper -- one that only makes calls -- and a looper waiting for a reply are
//! never handed a oneway call. One test per file: the broker is per host process.
mod binder_guest;

use binder_guest::*;

/// Read until there is nothing more for `t`.
fn drain(g: &Guest, t: &mut omni_linux::Task) -> Vec<Br> {
    let mut all = Vec::new();
    for _ in 0..16 {
        let got = g.read(t, &[]);
        if got.is_empty() {
            break;
        }
        all.extend(got);
    }
    all
}

#[test]
fn only_an_idle_looper_takes_the_processs_work() {
    let mut manager = Guest::new();
    let mut m = manager.looper();
    manager.set_context_mgr(&mut m);

    let mut receiver = Guest::new();
    let mut caller = receiver.thread(); // never enters the looper
    let mut waiting = receiver.looper();
    let mut idle = receiver.looper();

    // A looper of the receiver calls the manager and waits for the reply, handing it one of its
    // objects: the manager gets a handle to it.
    let call = receiver.transaction(BC_TRANSACTION, 0, 9, 0, &object(TYPE_BINDER, 0x100, 0x200), &[0]);
    assert_eq!(receiver.write_read(&mut waiting, &call, false).0, 0);
    let before = drain(&receiver, &mut waiting);
    assert!(cmds(&before).contains(&BR_TRANSACTION_COMPLETE), "{before:x?}");
    let got = manager.read(&mut m, &[]);
    assert_eq!(codes(&got), [9]);
    let handle = u32::from_le_bytes(got[0].data[8..12].try_into().unwrap());

    // The manager calls the object, oneway: work for the receiver's process.
    let oneway = manager.transaction(BC_TRANSACTION, handle, 1, TF_ONE_WAY, b"x", &[]);
    assert_eq!(manager.write_read(&mut m, &oneway, false).0, 0);

    assert!(codes(&receiver.read(&mut caller, &[])).is_empty(), "a thread that never entered the looper takes no process work");
    assert!(codes(&receiver.read(&mut waiting, &[])).is_empty(), "a looper waiting for a reply takes no process work");
    assert_eq!(codes(&receiver.read(&mut idle, &[])), [1], "an idle looper takes it");
}
