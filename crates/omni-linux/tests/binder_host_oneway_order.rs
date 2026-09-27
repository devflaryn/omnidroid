//! The host's oneway calls to a guest's object are ordered as anyone's: one at a time, the next
//! handed out once the guest frees the buffer of the one before (the kernel orders every oneway to
//! a node so, whoever sends it). The host's went straight to the process, so two host callbacks
//! (a composer's vsync and hotplug) could run at once on two binder threads, or pass a guest's
//! oneway to the same object. One test per file: the broker is per host process.
mod binder_guest;

use std::time::Duration;

use binder_guest::*;
use omni_linux::binder::{broker, Context, HostCall, HostReply};

/// Read until a return `want` comes (the host's calls arrive from other threads).
fn until(g: &Guest, t: &mut omni_linux::Task, cmds: &[u8], want: u32) -> Br {
    let mut first = cmds.to_vec();
    for _ in 0..2000 {
        let got = g.read(t, &first);
        first.clear();
        if let Some(b) = got.into_iter().find(|b| b.cmd == want) {
            return b;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("no {want:#x} came");
}

#[test]
fn the_hosts_oneway_calls_to_a_node_go_one_at_a_time() {
    let mut g = Guest::new();
    let mut t = g.looper();
    g.set_context_mgr(&mut t);

    // A host service the guest registers a callback with, as a client of a HAL does.
    let (handles_tx, handles_rx) = std::sync::mpsc::channel();
    let service = broker(Context::Binder).create_host_service_objects(move |call: HostCall| {
        let _ = handles_tx.send(call.handles.clone());
        HostReply::bytes(Vec::new())
    });
    let host = std::thread::spawn(move || broker(Context::Binder).host_transact(0, 1, object(TYPE_BINDER, service, service).to_vec(), &[0]));
    let given = until(&g, &mut t, &[], BR_TRANSACTION);
    let handle = u32::from_le_bytes(given.data[8..12].try_into().unwrap());
    let mut reply = free(given.buffer);
    reply.extend(g.transaction(BC_REPLY, 0, 0, 0, &[], &[]));
    assert_eq!(g.write_read(&mut t, &reply, false).0, 0);
    assert_eq!(host.join().unwrap(), Ok(Vec::new()));
    let register = g.transaction(BC_TRANSACTION, handle, 2, 0, &object(TYPE_BINDER, 0xcb, 0xcc), &[0]);
    let registered = until(&g, &mut t, &register, BR_REPLY);
    assert_eq!(g.write_read(&mut t, &free(registered.buffer), false).0, 0);
    let callback = handles_rx.recv_timeout(Duration::from_secs(10)).expect("the registration")[0];

    for code in [7, 8] {
        broker(Context::Binder).host_transact_oneway(callback, code, vec![code as u8], &[]).expect("oneway");
    }
    let first = until(&g, &mut t, &[], BR_TRANSACTION);
    assert_eq!(first.code, 7);
    assert!(codes(&g.read(&mut t, &[])).is_empty(), "the second waits while the first's buffer is held");
    let second = until(&g, &mut t, &free(first.buffer), BR_TRANSACTION);
    assert_eq!(second.code, 8, "the second once the first is freed");
}
