//! **The duplication census** (vendored patch 0106, arm64): a jit joined to a group reports each
//! block it emits, and -- asked -- every block it holds: at the end of its next run, or at once
//! while its thread is parked in an `SVC` callback. Never by stopping a running thread, and never
//! changing what the guest computes.
#![cfg(target_arch = "aarch64")]

mod harness;

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dynarmic_sys::*;
use harness::{a64, SvcPark, Vm, VmOptions, CODE_BASE, HALT_DONE};

/// What the sink received for one group: each call's kind and blocks, in order.
#[derive(Default)]
struct Received(Mutex<Vec<(u32, Vec<(u64, u32)>)>>);

impl Received {
    fn of(&self, kind: u32) -> Vec<Vec<(u64, u32)>> {
        self.0.lock().unwrap().iter().filter(|(k, _)| *k == kind).map(|(_, b)| b.clone()).collect()
    }
}

unsafe extern "C" fn sink(group: *mut c_void, kind: u32, locations: *const u64, host_bytes: *const u32, count: u64) {
    // SAFETY: every group joined in this file is a `Received` that outlives its jit; the shim
    // passes `count` elements in each array.
    let (group, locations, host_bytes) = unsafe {
        (
            &*group.cast::<Received>(),
            std::slice::from_raw_parts(locations, count as usize),
            std::slice::from_raw_parts(host_bytes, count as usize),
        )
    };
    group.0.lock().unwrap().push((kind, locations.iter().copied().zip(host_bytes.iter().copied()).collect()));
}

fn install_sink() {
    // SAFETY: `sink` is safe from any thread (it locks the group it is handed).
    unsafe { od_set_census_sink(Some(sink)) };
}

/// `ADD X0, X0, #1 ; B .+4`, `blocks` times (a block each), then `tail`.
fn chain(blocks: usize, tail: &[u32]) -> Vec<u32> {
    let mut code = Vec::new();
    for _ in 0..blocks {
        code.push(a64::add_imm(0, 0, 1));
        code.push(a64::b(1));
    }
    code.extend_from_slice(tail);
    code
}

const BLOCKS: usize = 100;

#[test]
fn a_joined_jit_reports_what_it_emits_and_at_the_end_of_a_run_what_it_holds() {
    install_sink();
    let group = Box::new(Received::default());
    let vm = Vm::new(chain(BLOCKS, &[a64::svc(0)]), VmOptions::default());
    // SAFETY: live, not yet run; `group` outlives the jit (dropped after `vm`, declared before).
    unsafe { od_jit_census_join(vm.raw(), std::ptr::from_ref::<Received>(&group).cast_mut().cast()) };
    vm.start(u64::MAX);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), BLOCKS as u64);

    let emitted: Vec<(u64, u32)> = group.of(OD_CENSUS_EMITTED).into_iter().flatten().collect();
    assert_eq!(emitted.len(), BLOCKS + 1, "each block reported once as it was emitted (the SVC's too)");
    assert!(emitted.iter().all(|&(_, bytes)| bytes > 0), "{emitted:?}");
    assert!(group.of(OD_CENSUS_LIVE).is_empty(), "nothing read before it was asked for");

    // Not parked (not even running): asked now, read by the thread at the end of its next run.
    // SAFETY: live.
    assert_eq!(unsafe { od_jit_block_census(vm.raw()) }, 0);
    assert!(group.of(OD_CENSUS_LIVE).is_empty());
    vm.set_pc(CODE_BASE + 4 * (2 * BLOCKS as u64));
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    let live = group.of(OD_CENSUS_LIVE);
    assert_eq!(live.len(), 1, "one read for one ask");
    let held: HashMap<u64, u32> = live[0].iter().copied().collect();
    let made: HashMap<u64, u32> = emitted.iter().copied().collect();
    assert_eq!(held, made, "it holds what it emitted, each with the size it was emitted with");

    // Asked once, read once.
    vm.set_pc(CODE_BASE + 4 * (2 * BLOCKS as u64));
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(group.of(OD_CENSUS_LIVE).len(), 1);

    // An invalidated block is no longer held.
    // SAFETY: live, not executing.
    unsafe { od_jit_invalidate_range(vm.raw(), CODE_BASE, 8) };
    vm.set_pc(CODE_BASE + 4 * (2 * BLOCKS as u64));
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    // SAFETY: live.
    unsafe { od_jit_block_census(vm.raw()) };
    vm.set_pc(CODE_BASE + 4 * (2 * BLOCKS as u64));
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    let live = group.of(OD_CENSUS_LIVE);
    assert_eq!(live.last().unwrap().len(), BLOCKS, "the first block's translation was dropped: {}", live.last().unwrap().len());
}

/// A thread of the chain, then `SVC #1` (parks), then `ADD X0, X0, #1000 ; SVC #0`, its blocks
/// asked for while it is parked; `ask_first`: asked once before it runs too, as by a look that found
/// it between runs -- an ask it answers only at the end of a run, and the `SVC` does not end one.
/// What the group had received by the end of the parked read, what it had at the end, and X0.
fn read_while_parked(ask_first: bool) -> (Vec<Vec<(u64, u32)>>, Arc<Received>, u64) {
    install_sink();
    let group = Arc::new(Received::default());
    let park = Arc::new(SvcPark::default());
    let code = chain(BLOCKS, &[a64::svc(1), a64::add_imm(0, 0, 1000), a64::svc(0)]);
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    let runner = {
        let (group, park) = (Arc::clone(&group), Arc::clone(&park));
        std::thread::spawn(move || {
            let vm = Vm::new(code, VmOptions::default());
            // SAFETY: live, not yet run; `group` (captured) outlives the jit.
            unsafe { od_jit_census_join(vm.raw(), Arc::as_ptr(&group).cast_mut().cast()) };
            vm.with_ctx(|c| c.park_on_svc1 = Arc::as_ptr(&park) as usize);
            if ask_first {
                // SAFETY: live.
                assert_eq!(unsafe { od_jit_block_census(vm.raw()) }, 0, "between runs: asked of its next run's end");
            }
            tx.send(vm.raw() as usize).unwrap();
            vm.start(u64::MAX);
            assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
            vm.reg(0)
        })
    };
    let jit = rx.recv().unwrap() as *mut c_void;
    park.wait_parked(1, Duration::from_secs(60));

    // SAFETY: the jit is live until the thread ends, which waits for the release below.
    assert_eq!(unsafe { od_jit_block_census(jit) }, 1, "read at once: its thread is parked");
    let live = group.of(OD_CENSUS_LIVE);
    park.release.store(true, std::sync::atomic::Ordering::SeqCst);
    let x0 = runner.join().unwrap();
    (live, group, x0)
}

#[test]
fn a_parked_thread_is_read_at_once_and_runs_on_with_nothing_cleared() {
    let (live, group, x0) = read_while_parked(false);
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].len(), BLOCKS + 1, "the chain and the parked SVC's block");
    assert_eq!(x0, BLOCKS as u64 + 1000, "the guest ran on to the end");
    let emitted = group.of(OD_CENSUS_EMITTED).into_iter().flatten().count();
    assert_eq!(emitted, BLOCKS + 2, "nothing was cleared and translated again: only the block after the SVC is new");
    assert_eq!(group.of(OD_CENSUS_LIVE).len(), 1, "and not read a second time");
}

/// An ask an earlier look left, which the thread had not answered when it parked, is withdrawn by
/// the parked read: the woken thread leaves its run at the read's halt without sending its blocks
/// again, so one look counts them once.
#[test]
fn a_parked_read_withdraws_an_earlier_ask() {
    let (live, group, x0) = read_while_parked(true);
    assert_eq!(live.len(), 1, "not answered before the read: the thread had not left its run");
    assert_eq!(x0, BLOCKS as u64 + 1000);
    assert_eq!(group.of(OD_CENSUS_LIVE).len(), 1, "read once: the woken thread did not answer the earlier ask too");
}
