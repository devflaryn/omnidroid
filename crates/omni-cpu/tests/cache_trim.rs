//! A thread's own translation cache (arm64) says what it holds, and a clear asked of it from
//! another thread empties it at its next run and gives its pages back (vendored patch 0102): what
//! `omni-linux`'s code trim does to a process gone quiet.
#![cfg(all(target_arch = "aarch64", feature = "dynarmic"))]

mod harness;

use std::sync::Arc;

use harness::a64::*;
use harness::x;
use omni_cpu::dynarmic::{DynarmicBackend, DynarmicOptions};
use omni_cpu::{ExitReason, GuestCpu, GuestRange, RunLimit};
use omni_mem::{CommitPolicy, Placement, Protection};

/// Distinct one-instruction blocks: some megabytes of translation, under the 64 MiB cache.
const CHAIN: usize = 60_000;

#[test]
fn a_cleared_cache_is_emptied_at_its_next_run_and_gives_its_pages_back() {
    let space = Arc::new(harness::high_guest_space());
    let mut p = Vec::new();
    for _ in 0..CHAIN {
        p.push(cbnz_w(1, 1)); // a block each (both ways lead to the next)
    }
    p.push(add_imm(3, 3, 1));
    p.push(ret(30));
    let code_bytes = ((p.len() + 4) * 4).next_multiple_of(space.page_size());
    let code = space
        .map_anonymous(Placement::Anywhere { align: space.page_size() }, code_bytes, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("a code region");
    let ptr = space.ptr(code, p.len() * 4).expect("the code");
    // SAFETY: committed read-write memory this test owns, `p.len() * 4` bytes of it.
    unsafe { core::ptr::copy_nonoverlapping(p.as_ptr(), ptr.cast::<u32>(), p.len()) };
    space.protect(code, code_bytes, Protection::ReadExecute).expect("executable");
    let tail = code + (CHAIN as u64 as usize) * 4; // `add`, `ret`: a short run

    let options = DynarmicOptions { code_cache_size: 64 << 20, ..DynarmicOptions::default() };
    let backend = DynarmicBackend::new(Arc::clone(&space), options).expect("a backend");
    backend.invalidate_code(GuestRange::new(code, p.len() * 4).expect("a range"));
    let mut cpu = backend.create_thread_with_tls().expect("a thread");
    let sentinel = code + code_bytes - 4;
    cpu.set_return_sentinel(sentinel).expect("the sentinel");

    cpu.set_x(x(30), sentinel as u64);
    assert_eq!(cpu.run(code, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    let full = backend.code_cache_stats().expect("per-thread caches are counted").committed_bytes;
    let emitted = backend.code_cache_stats().unwrap().code_bytes_emitted;
    assert!(full > 1 << 20, "{full} bytes held after {CHAIN} blocks");
    assert!(emitted >= full);
    let footprint = omni_platform::vm::process_commit_charge().expect("the footprint");

    // Asked from this thread (it could be any); applied when the thread next runs.
    backend.clear_code_cache();
    cpu.set_x(x(30), sentinel as u64);
    assert_eq!(cpu.run(tail, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    let after = backend.code_cache_stats().unwrap().committed_bytes;
    assert!(after < 64 << 10, "only the short run's translation is held: {after} bytes");
    let dropped = footprint.saturating_sub(omni_platform::vm::process_commit_charge().expect("the footprint"));
    assert!(dropped as u64 >= full / 2, "the pages went back: footprint -{dropped} for {full} bytes of translations");

    // And the cache works on: the whole chain again, translated anew.
    cpu.set_x(x(30), sentinel as u64);
    cpu.set_x(x(3), 0);
    assert_eq!(cpu.run(code, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    assert_eq!(cpu.x(x(3)), 1);
    assert!(backend.code_cache_stats().unwrap().committed_bytes > full / 2);
}

static PARKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The guest's `SVC`: says it is parked, and waits until released (a system call that blocks).
fn park(call: &mut omni_cpu::ThunkCall<'_>) {
    use std::sync::atomic::Ordering;
    PARKED.store(true, Ordering::SeqCst);
    while !RELEASE.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    call.set_x(0, 42);
}

/// A thread parked in a system call has its translated code given back by another thread (vendored
/// patch 0104), and carries on after it: the rest of the `SVC`'s block runs, the run ends at its
/// halt, and the code after it is translated again.
#[test]
fn a_parked_thread_s_code_is_given_back_and_it_carries_on() {
    use std::sync::atomic::Ordering;
    let space = Arc::new(harness::high_guest_space());
    let mut p = Vec::new();
    for _ in 0..CHAIN {
        p.push(cbnz_w(1, 1));
    }
    p.push(movz(0, 0, 0));
    p.push(svc(0)); // parks, then x0 = 42
    for _ in 0..1000 {
        p.push(cbnz_w(1, 1)); // translated after the discard
    }
    p.push(add_imm(3, 0, 1)); // x3 = 43
    p.push(ret(30));
    let code_bytes = ((p.len() + 4) * 4).next_multiple_of(space.page_size());
    let code = space
        .map_anonymous(Placement::Anywhere { align: space.page_size() }, code_bytes, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("a code region");
    let ptr = space.ptr(code, p.len() * 4).expect("the code");
    // SAFETY: committed read-write memory this test owns, `p.len() * 4` bytes of it.
    unsafe { core::ptr::copy_nonoverlapping(p.as_ptr(), ptr.cast::<u32>(), p.len()) };
    space.protect(code, code_bytes, Protection::ReadExecute).expect("executable");

    let options = DynarmicOptions { code_cache_size: 64 << 20, ..DynarmicOptions::default() };
    let backend = DynarmicBackend::new(Arc::clone(&space), options).expect("a backend");
    backend.invalidate_code(GuestRange::new(code, p.len() * 4).expect("a range"));
    let sentinel = code + code_bytes - 4;
    std::thread::scope(|scope| {
        let runner = scope.spawn(|| {
            let mut cpu = backend.create_thread_with_tls().expect("a thread");
            cpu.set_return_sentinel(sentinel).expect("the sentinel");
            cpu.set_svc_handler(park, omni_cpu::ThunkContext(0)).expect("a syscall handler");
            cpu.set_x(x(30), sentinel as u64);
            let mut exit = cpu.run(code, RunLimit::Unlimited).expect("runs");
            // The clear's halt may end the run at the SVC's block; the guest goes on from there.
            while exit != (ExitReason::Returned { pc: sentinel }) {
                let pc = cpu.pc();
                exit = cpu.run(pc, RunLimit::Unlimited).expect("runs on");
            }
            (cpu.x(x(0)), cpu.x(x(3)))
        });
        while !PARKED.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let held = backend.code_cache_stats().expect("counted").committed_bytes;
        assert!(held > 1 << 20, "{held} bytes held when parked");
        let given = backend.discard_parked(std::time::Duration::ZERO);
        assert!(given as u64 >= held / 2, "{given} of {held} bytes given back");
        RELEASE.store(true, Ordering::SeqCst);
        assert_eq!(runner.join().expect("the guest thread"), (42, 43), "the rest ran after the discard");
    });
}

/// How many times the test below has parked.
static TABLES_PARKED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static TABLES_RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// [`park`]'s twin for the test below (tests run side by side).
fn park_tables(call: &mut omni_cpu::ThunkCall<'_>) {
    use std::sync::atomic::Ordering;
    TABLES_PARKED.fetch_add(1, Ordering::SeqCst);
    while !TABLES_RELEASE.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    call.set_x(0, 42);
}

/// A parked thread's per-block tables go with its code (vendored patch 0107), it runs on and
/// translates again; and, still parked, it is given back once (0108): a second pass finds nothing,
/// where it used to count the same pages again every minute. Once it has run, parked again, it is
/// given back again.
#[test]
fn a_parked_thread_gives_back_its_tables_and_runs_on() {
    use std::sync::atomic::Ordering;
    let space = Arc::new(harness::high_guest_space());
    let mut p = Vec::new();
    for _ in 0..CHAIN {
        p.push(cbnz_w(1, 1));
    }
    p.push(movz(0, 0, 0));
    p.push(svc(0)); // parks, then x0 = 42
    for _ in 0..1000 {
        p.push(cbnz_w(1, 1)); // translated after the discard
    }
    p.push(add_imm(3, 0, 1)); // x3 = 43
    p.push(ret(30));
    let code_bytes = ((p.len() + 4) * 4).next_multiple_of(space.page_size());
    let code = space
        .map_anonymous(Placement::Anywhere { align: space.page_size() }, code_bytes, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("a code region");
    let ptr = space.ptr(code, p.len() * 4).expect("the code");
    // SAFETY: committed read-write memory this test owns, `p.len() * 4` bytes of it.
    unsafe { core::ptr::copy_nonoverlapping(p.as_ptr(), ptr.cast::<u32>(), p.len()) };
    space.protect(code, code_bytes, Protection::ReadExecute).expect("executable");

    let options = DynarmicOptions { code_cache_size: 64 << 20, ..DynarmicOptions::default() };
    let backend = DynarmicBackend::new(Arc::clone(&space), options).expect("a backend");
    backend.invalidate_code(GuestRange::new(code, p.len() * 4).expect("a range"));
    let sentinel = code + code_bytes - 4;
    std::thread::scope(|scope| {
        let runner = scope.spawn(|| {
            let mut cpu = backend.create_thread_with_tls().expect("a thread");
            cpu.set_return_sentinel(sentinel).expect("the sentinel");
            cpu.set_svc_handler(park_tables, omni_cpu::ThunkContext(0)).expect("a syscall handler");
            cpu.set_x(x(30), sentinel as u64);
            let mut exit = cpu.run(code, RunLimit::Unlimited).expect("runs");
            while exit != (ExitReason::Returned { pc: sentinel }) {
                let pc = cpu.pc();
                exit = cpu.run(pc, RunLimit::Unlimited).expect("runs on");
            }
            let emitted = backend.code_cache_stats().unwrap().code_bytes_emitted;
            // The chain again, from the start: translated anew, its tables made again, and parked
            // again at the same `SVC`.
            cpu.set_x(x(30), sentinel as u64);
            TABLES_RELEASE.store(false, Ordering::SeqCst);
            let mut exit = cpu.run(code, RunLimit::Unlimited).expect("runs");
            while exit != (ExitReason::Returned { pc: sentinel }) {
                let pc = cpu.pc();
                exit = cpu.run(pc, RunLimit::Unlimited).expect("runs on");
            }
            let again = backend.code_cache_stats().unwrap().code_bytes_emitted - emitted;
            (cpu.x(x(0)), cpu.x(x(3)), again)
        });
        while TABLES_PARKED.load(Ordering::SeqCst) < 1 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let tables = backend.table_bytes();
        let held = backend.code_cache_stats().expect("counted").committed_bytes;
        assert!(tables > 1 << 20, "{tables} bytes of tables held when parked");
        let given = backend.discard_parked(std::time::Duration::ZERO);
        assert!(given as u64 >= held / 2, "{given} of {held} bytes given back");
        let left = backend.table_bytes();
        assert!(left < 16 << 10, "the tables went with the code: {left} of {tables} bytes left");
        assert_eq!(backend.discard_parked(std::time::Duration::ZERO), 0, "given back once: nothing more while it stays parked");
        TABLES_RELEASE.store(true, Ordering::SeqCst);
        // Its clear ended the given-back state: parked again, it has tables and code to give again.
        while TABLES_PARKED.load(Ordering::SeqCst) < 2 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let tables_again = backend.table_bytes();
        let given_again = backend.discard_parked(std::time::Duration::ZERO);
        TABLES_RELEASE.store(true, Ordering::SeqCst);
        let (x0, x3, again) = runner.join().expect("the guest thread");
        assert_eq!((x0, x3), (42, 43), "the rest ran after the discard");
        assert!(again >= held / 2, "the chain was translated again: {again} bytes (held {held})");
        assert!(tables_again > tables / 2, "and its tables made again: {tables_again} bytes (were {tables})");
        assert!(given_again as u64 >= held / 2, "run since, it is given back again: {given_again} bytes (held {held})");
    });
}

static ONE_PARKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ONE_RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// [`park`]'s twin for the test below.
fn park_one(call: &mut omni_cpu::ThunkCall<'_>) {
    use std::sync::atomic::Ordering;
    ONE_PARKED.store(true, Ordering::SeqCst);
    while !ONE_RELEASE.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    call.set_x(0, 42);
}

/// One thread's translations dropped (`omni-linux`'s quiet threads), the thread parked in a system
/// call: given back at once, tables too, rather than at a clear that would wait for it to wake --
/// and a parked pass after finds nothing more of it to count.
#[test]
fn one_parked_thread_s_translations_are_given_back_at_once() {
    use std::sync::atomic::Ordering;
    let space = Arc::new(harness::high_guest_space());
    let mut p = Vec::new();
    for _ in 0..CHAIN {
        p.push(cbnz_w(1, 1));
    }
    p.push(movz(0, 0, 0));
    p.push(svc(0)); // parks, then x0 = 42
    p.push(add_imm(3, 0, 1)); // x3 = 43
    p.push(ret(30));
    let code_bytes = ((p.len() + 4) * 4).next_multiple_of(space.page_size());
    let code = space
        .map_anonymous(Placement::Anywhere { align: space.page_size() }, code_bytes, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("a code region");
    let ptr = space.ptr(code, p.len() * 4).expect("the code");
    // SAFETY: committed read-write memory this test owns, `p.len() * 4` bytes of it.
    unsafe { core::ptr::copy_nonoverlapping(p.as_ptr(), ptr.cast::<u32>(), p.len()) };
    space.protect(code, code_bytes, Protection::ReadExecute).expect("executable");

    let options = DynarmicOptions { code_cache_size: 64 << 20, ..DynarmicOptions::default() };
    let backend = DynarmicBackend::new(Arc::clone(&space), options).expect("a backend");
    backend.invalidate_code(GuestRange::new(code, p.len() * 4).expect("a range"));
    let sentinel = code + code_bytes - 4;
    std::thread::scope(|scope| {
        let runner = scope.spawn(|| {
            let mut cpu = backend.create_thread_with_tls().expect("a thread");
            cpu.set_return_sentinel(sentinel).expect("the sentinel");
            cpu.set_svc_handler(park_one, omni_cpu::ThunkContext(0)).expect("a syscall handler");
            cpu.set_x(x(30), sentinel as u64);
            let mut exit = cpu.run(code, RunLimit::Unlimited).expect("runs");
            while exit != (ExitReason::Returned { pc: sentinel }) {
                let pc = cpu.pc();
                exit = cpu.run(pc, RunLimit::Unlimited).expect("runs on");
            }
            (cpu.x(x(0)), cpu.x(x(3)))
        });
        while !ONE_PARKED.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let caches = backend.thread_caches();
        assert_eq!(caches.len(), 1);
        let (key, held, _) = caches[0];
        assert!(held > 1 << 20, "{held} bytes held when parked");
        let given = backend.clear_thread_code(key).expect("its thread");
        assert!(given >= held / 2, "given back at once: {given} of {held} bytes");
        assert!(backend.table_bytes() < 16 << 10, "its tables with it");
        assert_eq!(backend.discard_parked(std::time::Duration::ZERO), 0, "nothing more for the parked pass to count");
        ONE_RELEASE.store(true, Ordering::SeqCst);
        assert_eq!(runner.join().expect("the guest thread"), (42, 43), "the rest ran after it");
    });
    assert_eq!(backend.clear_thread_code(usize::MAX), None, "no such thread");
}
