//! An object in flight is not released: a transaction holds the nodes it names (and its target)
//! from when it is sent until its receiver frees the buffer, as the kernel's node references for a
//! transaction do (`binder_inc_node`, dropped by `binder_transaction_buffer_release`). A process
//! that sent system_server a handle to one of system_server's own objects and died before
//! system_server read it had the object released under it: system_server freed it, then read the
//! transaction, and its `Parcel::unflattenBinder` used the freed object (a SIGSEGV in
//! `RefBase::incStrongRequireStrong`). One test per file: the broker is per host process.
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
fn an_object_in_flight_is_not_released_before_its_buffer_is_freed() {
    let mut holder = Guest::new();
    let mut h = holder.looper();
    holder.set_context_mgr(&mut h);
    let mut owner = Guest::new();
    let mut o = owner.looper();

    // The owner hands the holder its object, and says it holds it for the holder.
    let give = owner.transaction(BC_TRANSACTION, 0, 1, TF_ONE_WAY, &object(TYPE_BINDER, 0x5000, 0x6000), &[0]);
    let got = owner.read(&mut o, &give);
    let mut got = [got, drain(&owner, &mut o)].concat();
    assert!(cmds(&got).contains(&BR_ACQUIRE), "{got:x?}");
    let mut done = BC_ACQUIRE_DONE.to_le_bytes().to_vec();
    done.extend_from_slice(&0x5000u64.to_le_bytes());
    done.extend_from_slice(&0x6000u64.to_le_bytes());
    assert_eq!(owner.write_read(&mut o, &done, false).0, 0);
    let given = holder.read(&mut h, &[]);
    assert_eq!(codes(&given), [1]);
    let handle = u32::from_le_bytes(given[0].data[8..12].try_into().unwrap());
    assert_eq!(holder.write_read(&mut h, &free(given[0].buffer), false).0, 0);

    // The holder sends the owner its own object back, then dies before the owner reads it.
    let back = holder.transaction(BC_TRANSACTION, handle, 2, TF_ONE_WAY, &object(TYPE_HANDLE, u64::from(handle), 0), &[0]);
    assert_eq!(cmds(&holder.read(&mut h, &back)), [BR_TRANSACTION_COMPLETE]);
    holder.close(&mut h);
    drop(holder);

    got = drain(&owner, &mut o);
    assert_eq!(codes(&got), [2], "the transaction still comes: {got:x?}");
    assert!(!cmds(&got).contains(&BR_RELEASE) && !cmds(&got).contains(&BR_DECREFS), "no release while the object is in flight: {got:x?}");
    let txn = got.iter().find(|b| b.cmd == BR_TRANSACTION).unwrap();
    assert_eq!(&txn.data[0..4], &TYPE_BINDER.to_le_bytes(), "the owner's own object arrives as its binder");
    assert_eq!(&txn.data[16..24], &0x6000u64.to_le_bytes());

    // Its buffer freed, the object -- no live process refers to it -- is let go.
    let after = [owner.read(&mut o, &free(txn.buffer)), drain(&owner, &mut o)].concat();
    assert!(cmds(&after).contains(&BR_RELEASE) && cmds(&after).contains(&BR_DECREFS), "released once its buffer is freed: {after:x?}");
}
