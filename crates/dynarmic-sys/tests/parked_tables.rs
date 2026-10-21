//! **A parked thread's tables, given back with its code** (vendored patch 0107, arm64): while a
//! thread waits in an `SVC` callback, another thread gives back its translated code (0104) and now
//! its per-block tables too. Safe because the woken thread runs only the rest of the `SVC`'s block,
//! whose `CheckHalt` leaves the run before any lookup: no dispatcher lookup and no fastmem fault
//! happens between its waking and its clear -- counted here by the backend's own hook. And (patch
//! 0108) a thread already given back, and not run since, has nothing more to give; once it has run,
//! it counts again.
//!
//! The hook's count is the process's, so the tests here take turns ([`TURN`]): a lookup made by
//! one while another reads the count would be blamed on the wrong thread, or hide a missing one.
#![cfg(target_arch = "aarch64")]

mod harness;

use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dynarmic_sys::*;
use harness::{a64, SvcPark, Vm, VmOptions, CODE_BASE, HALT_DONE};

const BLOCKS: usize = 4_000;

/// `LDR X3, [X2] ; ADD X0, X0, #1 ; B .+4` x [`BLOCKS`] -- a fastmem site and a link each -- then
/// `SVC #1` (parks), then `ADD X0, X0, #1000 ; SVC #0`.
fn program() -> Vec<u32> {
    let mut code = Vec::new();
    for _ in 0..BLOCKS {
        code.push(a64::ldr_imm(3, 2, 0));
        code.push(a64::add_imm(0, 0, 1));
        code.push(a64::b(1));
    }
    code.push(a64::svc(1));
    code.push(a64::add_imm(0, 0, 1000));
    code.push(a64::svc(0));
    code
}

/// Where the `SVC #1` is.
const SVC1: u64 = CODE_BASE + 4 * (3 * BLOCKS as u64);

fn code_bytes(jit: *mut c_void) -> (u64, u64) {
    let (mut in_use, mut emitted) = (0, 0);
    // SAFETY: `jit` is live; the counters are atomics.
    unsafe { od_jit_code_bytes(jit, &mut in_use, &mut emitted) };
    (in_use, emitted)
}

/// The tests' turns at the give-back hook's count (see the file's head).
static TURN: Mutex<()> = Mutex::new(());

/// A thread of [`program`], parked at its `SVC #1`; `then` runs on the main thread while it is, and
/// the thread is released after. Then the chain once more from the start, parked at the same `SVC`
/// again and given back again by the main thread. What the thread saw: the first run's halt, PC and
/// X0, the give-back hook's count after that run and at the end, X0 at the end of the first pass,
/// and its emitted bytes before and after the second; and what the second give-back gave.
struct Woken {
    first: u32,
    pc: u64,
    x0: u64,
    lookups_after: u64,
    lookups_end: u64,
    end_x0: u64,
    emitted_before_again: u64,
    emitted_after_again: u64,
    given_again: u64,
}

fn parked_then(then: impl FnOnce(*mut c_void)) -> Woken {
    let _turn = TURN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (park, park_again) = (Arc::new(SvcPark::default()), Arc::new(SvcPark::default()));
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    let runner = {
        let (park, park_again) = (Arc::clone(&park), Arc::clone(&park_again));
        std::thread::spawn(move || {
            let vm = Vm::new(program(), VmOptions { cycle_counting: true, code_cache_size: 32 << 20, ..VmOptions::default() });
            vm.with_ctx(|c| {
                c.park_on_svc1 = Arc::as_ptr(&park) as usize;
                c.write_u64(0x100, 7);
            });
            vm.set_reg(2, 0x100);
            tx.send(vm.raw() as usize).unwrap();
            vm.start(u64::MAX >> 2);
            let first = vm.run();
            let (pc, x0) = (vm.pc(), vm.reg(0));
            // SAFETY: no arguments; an atomic counter.
            let lookups_after = unsafe { od_lookups_after_give_back() };
            // SAFETY: live, not executing.
            unsafe { od_jit_clear_halt(vm.raw(), OD_HALT_CACHE_INVALIDATION | HALT_DONE) };
            // (Not asserted here: a run that did not stop at the halt has finished already.)
            if first & HALT_DONE == 0 {
                vm.run_to_completion(64);
            }
            let end_x0 = vm.reg(0);
            // The whole chain once more, translated again if its translations went, and parked
            // again: a thread that has run since its give-back has something to give again.
            let emitted_before_again = code_bytes(vm.raw()).1;
            vm.with_ctx(|c| c.park_on_svc1 = Arc::as_ptr(&park_again) as usize);
            vm.start(u64::MAX >> 2);
            assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
            let emitted_after_again = code_bytes(vm.raw()).1;
            // SAFETY: as above.
            let lookups_end = unsafe { od_lookups_after_give_back() };
            // (`park` and `park_again`, captured, outlive `vm`.)
            Woken { first, pc, x0, lookups_after, lookups_end, end_x0, emitted_before_again, emitted_after_again, given_again: 0 }
        })
    };
    let jit = rx.recv().unwrap() as *mut c_void;
    park.wait_parked(1, Duration::from_secs(60));
    then(jit);
    park.release.store(true, std::sync::atomic::Ordering::SeqCst);
    park_again.wait_parked(1, Duration::from_secs(60));
    // SAFETY: `jit` is live until its thread ends, which waits for the release below.
    let given_again = unsafe { od_jit_discard_parked(jit, 0) };
    park_again.release.store(true, std::sync::atomic::Ordering::SeqCst);
    Woken { given_again, ..runner.join().expect("the parked thread") }
}

#[test]
fn a_discarded_thread_takes_no_fault_or_lookup_before_its_clear() {
    let mut seen = (0, 0, 0, 0, 0);
    let woken = parked_then(|jit| {
        // SAFETY: `jit` is live until its thread ends, which waits for the release.
        let tables = unsafe { od_jit_table_bytes(jit) };
        let held = code_bytes(jit).0;
        // SAFETY: as above; no arguments.
        let lookups = unsafe { od_lookups_after_give_back() };
        // SAFETY: as above.
        let given = unsafe { od_jit_discard_parked(jit, 0) };
        // SAFETY: as above.
        seen = (tables, held, lookups, given, unsafe { od_jit_table_bytes(jit) });
    });
    let (tables, held, lookups, given, tables_after) = seen;
    eprintln!("parked: {held} bytes of code, {tables} of tables; given back {given} of code, tables then {tables_after}");
    assert!(tables > 256 << 10, "the chain's tables were counted: {tables} bytes");
    assert!(given >= held / 2, "{given} of {held} bytes of code given back");
    assert!(tables_after < 4 << 10, "the tables went with the code: {tables_after} bytes left of {tables}");

    assert_eq!(woken.lookups_after, lookups, "no lookup and no fault between its waking and its clear");
    assert_ne!(woken.first & OD_HALT_CACHE_INVALIDATION, 0, "the woken run ended at the discard's halt: {:#x}", woken.first);
    assert_eq!((woken.pc, woken.x0), (SVC1 + 4, BLOCKS as u64), "it stopped right after the SVC's block");
    assert_eq!(woken.end_x0, BLOCKS as u64 + 1000, "and the guest ran on, right");
    let again = woken.emitted_after_again - woken.emitted_before_again;
    assert!(again >= held / 2, "the chain was translated again: {again} bytes (held {held})");
    assert_eq!(woken.lookups_end, woken.lookups_after, "its clear ended the given-back state: the lookups since are not counted");
    assert!(woken.given_again >= held / 2, "run since, it gave back again: {} bytes (held {held})", woken.given_again);
}

#[test]
fn a_thread_still_parked_gives_its_code_back_once() {
    let mut given = (0, 0);
    let woken = parked_then(|jit| {
        // SAFETY: `jit` is live until its thread ends, which waits for the release.
        given = unsafe { (od_jit_discard_parked(jit, 0), od_jit_discard_parked(jit, 0)) };
    });
    assert!(given.0 > 128 << 10, "the first discard gave its code back: {} bytes", given.0);
    assert_eq!(given.1, 0, "the second found nothing more: it has not run since");
    assert_eq!(woken.end_x0, BLOCKS as u64 + 1000);
    assert!(woken.given_again > 128 << 10, "once it has run, it counts again: {} bytes", woken.given_again);
}
