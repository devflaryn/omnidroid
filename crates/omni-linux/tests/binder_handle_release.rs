//! A handle a live process lets go is deleted, and an object no one refers to any more is released
//! by its owner (`BR_RELEASE`, `BR_DECREFS`) -- as the kernel deletes a `binder_ref` whose counts
//! reach zero. Before, a live process's handles lasted as long as it did: system_server, which
//! never ends, kept every SurfaceFlinger layer handle WindowManager was ever given, and every
//! app's layers and last buffers stayed after the app ended. One test per file: the broker is per
//! host process.
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
fn a_handle_let_go_by_a_live_process_releases_its_object() {
    let mut holder = Guest::new();
    let mut h = holder.looper();
    holder.set_context_mgr(&mut h);
    let mut owner = Guest::new();
    let mut o = owner.looper();

    // The owner hands the holder its object; the owner holds it for the holder.
    let give = owner.transaction(BC_TRANSACTION, 0, 1, TF_ONE_WAY, &object(TYPE_BINDER, 0x5000, 0x6000), &[0]);
    let got = [owner.read(&mut o, &give), drain(&owner, &mut o)].concat();
    assert!(cmds(&got).contains(&BR_ACQUIRE), "{got:x?}");
    let mut done = BC_ACQUIRE_DONE.to_le_bytes().to_vec();
    done.extend_from_slice(&0x5000u64.to_le_bytes());
    done.extend_from_slice(&0x6000u64.to_le_bytes());
    assert_eq!(owner.write_read(&mut o, &done, false).0, 0);

    // The holder keeps the handle, as libbinder does (a BpBinder made for it), and frees the buffer.
    let given = holder.read(&mut h, &[]);
    assert_eq!(codes(&given), [1]);
    let handle = u32::from_le_bytes(given[0].data[8..12].try_into().unwrap());
    assert_eq!(holder.write_read(&mut h, &[acquire(handle), free(given[0].buffer)].concat(), false).0, 0);
    let kept = drain(&owner, &mut o);
    assert!(!cmds(&kept).contains(&BR_RELEASE), "held while the holder keeps it: {kept:x?}");

    // Its weak reference alone still holds it.
    let strong_only = [BC_RELEASE.to_le_bytes(), handle.to_le_bytes()].concat();
    assert_eq!(holder.write_read(&mut h, &strong_only, false).0, 0);
    let weak_left = drain(&owner, &mut o);
    assert!(!cmds(&weak_left).contains(&BR_RELEASE) && !cmds(&weak_left).contains(&BR_DECREFS), "{weak_left:x?}");

    // The last one gone, the holder -- alive -- refers to it no more: the owner lets it go.
    let weak = [BC_DECREFS.to_le_bytes(), handle.to_le_bytes()].concat();
    assert_eq!(holder.write_read(&mut h, &weak, false).0, 0);
    let after = drain(&owner, &mut o);
    assert!(cmds(&after).contains(&BR_RELEASE) && cmds(&after).contains(&BR_DECREFS), "released: {after:x?}");

    // The handle is gone: a transaction on it fails, as on any handle the process does not have.
    let on_it = holder.transaction(BC_TRANSACTION, handle, 2, TF_ONE_WAY, &[], &[]);
    let _ = holder.read(&mut h, &on_it);
    let reached = drain(&owner, &mut o);
    assert!(!codes(&reached).contains(&2), "nothing reaches the owner through a deleted handle: {reached:x?}");
}
