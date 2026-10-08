//! **What one call to a host service costs** the guest thread that makes it, at the driver: a
//! blocking `BC_TRANSACTION` to a host service and the wait for its `BR_REPLY`, as SurfaceFlinger
//! makes each `executeCommands` to the composer (twice a frame while the composer answers
//! `presentOrValidateDisplay` with "validated"). Timing, so ignored by default:
//! `cargo test --release -p omni-linux --test binder_host_roundtrip -- --ignored --nocapture`.
//! The guest's own marshalling (libbinder, translated) is not in it.
//! One test per file: the broker is per host process.
mod binder_guest;

use std::time::{Duration, Instant};

use binder_guest::*;
use omni_linux::binder::{broker, Context, HostCall, HostReply};

#[test]
#[ignore = "timing; run by hand"]
fn a_host_service_round_trip() {
    let mut g = Guest::new();
    let mut t = g.looper();
    g.set_context_mgr(&mut t);
    // A host service answering ~1 KiB, about a frame's command results.
    let service = broker(Context::Binder).create_host_service_objects(|_call: HostCall| HostReply::bytes(vec![0u8; 1024]));
    let host = std::thread::spawn(move || broker(Context::Binder).host_transact(0, 1, object(TYPE_BINDER, service, service).to_vec(), &[0]));
    let given = loop {
        if let Some(b) = g.read(&mut t, &[]).into_iter().find(|b| b.cmd == BR_TRANSACTION) {
            break b;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let handle = u32::from_le_bytes(given.data[8..12].try_into().unwrap());
    let mut reply = free(given.buffer);
    reply.extend(g.transaction(BC_REPLY, 0, 0, 0, &[], &[]));
    assert_eq!(g.write_read(&mut t, &reply, false).0, 0);
    host.join().unwrap().unwrap();

    // ~2 KiB of commands, about a frame's.
    let parcel = vec![7u8; 2048];
    for pool in ["0", "1"] {
        omni_linux::lever::apply(&format!("binder_host_pool={pool}")).unwrap();
        let mut took = Vec::new();
        for i in 0..2200 {
            let start = Instant::now();
            let mut cmds = g.transaction(BC_TRANSACTION, handle, 1, 0, &parcel, &[]);
            let buffer = loop {
                // Spins on EAGAIN (the descriptor is non-blocking): the guest thread's own wait
                // is not in the figure, only the host's work and hand-offs.
                if let Some(b) = g.read(&mut t, &cmds).into_iter().find(|b| b.cmd == BR_REPLY) {
                    break b.buffer;
                }
                cmds.clear();
                std::hint::spin_loop();
            };
            if i >= 200 {
                took.push(start.elapsed());
            }
            assert_eq!(g.write_read(&mut t, &free(buffer), false).0, 0);
            // A frame's gap, so a kept thread is back waiting.
            std::thread::sleep(Duration::from_micros(500));
        }
        took.sort();
        eprintln!(
            "[binder] host-service round trip, binder_host_pool={pool}: p50 {:?} p90 {:?} mean {:?} (n={})",
            took[took.len() / 2],
            took[took.len() * 9 / 10],
            took.iter().sum::<Duration>() / took.len() as u32,
            took.len()
        );
    }
}
