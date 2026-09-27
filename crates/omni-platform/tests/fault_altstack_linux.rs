//! The alternate signal stack a thread that takes guest faults runs its handler on.
//!
//! The Linux handler is installed `SA_ONSTACK`, and a `std::thread` has an alternate stack of
//! `max(SIGSTKSZ, AT_MINSIGSTKSZ)` -- 8 KiB on x86-64 -- over a guard page. The demand pager's path
//! needs more than that in a debug build (12,496 bytes measured, `omni-mem/tests/pager_linux`), and a
//! handler that overruns it faults on the guard page with `SIGSEGV` blocked, which the kernel turns
//! into the death of the whole process. `fault::prepare_thread` gives the calling thread an
//! alternate stack of [`fault::ALTERNATE_STACK_BYTES`]. A handler here takes 32 KiB of stack, so the
//! result does not depend on how deep a debug pager happens to be.
#![cfg(target_os = "linux")]

use std::os::unix::process::ExitStatusExt;

use omni_platform::fault::{self, Fault, FaultOutcome};
use omni_platform::vm::{self, Protection};

const PAGE: usize = 4096;
/// More than a std thread's alternate stack, less than the one `prepare_thread` installs.
const HANDLER_FRAME: usize = 32 * 1024;
/// Set in the child process that is expected to die.
const CHILD: &str = "OMNI_FAULT_ALTSTACK_CHILD";

struct Region {
    base: usize,
    len: usize,
}

#[inline(never)]
fn deep(context: usize, fault: &Fault) -> FaultOutcome {
    // SAFETY: `context` is a leaked `Region`, valid for the whole process.
    let region: &Region = unsafe { &*(context as *const Region) };
    if fault.address < region.base || fault.address >= region.base + region.len {
        return FaultOutcome::NotOurs;
    }
    let mut frame = [0u8; HANDLER_FRAME];
    frame[HANDLER_FRAME - 1] = 1;
    std::hint::black_box(&mut frame);
    let page = fault.address & !(PAGE - 1);
    // SAFETY: the page lies inside a live reservation this test leaks.
    match unsafe { vm::commit(page as *mut u8, PAGE, Protection::ReadWrite) } {
        Ok(()) => FaultOutcome::Resolved,
        Err(_) => FaultOutcome::NotOurs,
    }
}

/// Reserve a page, install `deep` for it, and touch it on a new std thread.
fn fault_on_a_std_thread(prepare: bool) {
    // Never released: the handler may be asked about it for the rest of the process.
    let base = vm::reserve(PAGE, PAGE).expect("reserve a page").base();
    let region: &'static Region = Box::leak(Box::new(Region { base, len: PAGE }));
    // SAFETY: `deep` does not unwind or lock, and its context is leaked.
    let registration = unsafe { fault::install(deep, region as *const Region as usize) }.expect("install");
    std::mem::forget(registration);
    std::thread::spawn(move || {
        if prepare {
            fault::prepare_thread().expect("prepare the thread");
        }
        // SAFETY: the page is reserved; the handler commits it on the first touch.
        unsafe { core::ptr::write_volatile(base as *mut u8, 7) };
        // SAFETY: committed by the handler.
        assert_eq!(unsafe { core::ptr::read_volatile(base as *const u8) }, 7);
    })
    .join()
    .expect("the faulting thread");
}

/// A prepared thread has an alternate stack of at least `ALTERNATE_STACK_BYTES`, and a handler
/// deeper than a std thread's own serves a fault on it.
#[test]
fn a_prepared_thread_serves_a_deep_handler() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    std::thread::spawn(|| {
        fault::prepare_thread().expect("prepare");
        fault::prepare_thread().expect("idempotent");
        // SAFETY: plain data, filled in by a query-only call.
        let mut current: libc::stack_t = unsafe { core::mem::zeroed() };
        // SAFETY: a NULL new stack only reads the current one.
        assert_eq!(unsafe { libc::sigaltstack(core::ptr::null(), &mut current) }, 0);
        assert_eq!(current.ss_flags & libc::SS_DISABLE, 0, "enabled");
        assert!(current.ss_size >= fault::ALTERNATE_STACK_BYTES, "{} bytes", current.ss_size);
    })
    .join()
    .expect("the prepared thread");
    fault_on_a_std_thread(true);
}

/// Without it the same handler overruns the std alternate stack and the process dies by `SIGSEGV`
/// -- the failure mode this exists for, shown in a child process.
#[test]
fn an_unprepared_std_thread_dies_in_a_deep_handler() {
    if std::env::var_os(CHILD).is_some() {
        fault_on_a_std_thread(false);
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().expect("this binary"))
        .args(["--exact", "an_unprepared_std_thread_dies_in_a_deep_handler", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run the child");
    assert_eq!(status.signal(), Some(libc::SIGSEGV), "{status}");
}
