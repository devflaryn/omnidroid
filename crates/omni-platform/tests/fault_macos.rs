//! The guest-fault seam on macOS, asserted by faulting for real.
//!
//! Each test that can fault in a way the process does not survive runs it in a **child** of this
//! binary. The rest fault in-process on memory the test owns, with a handler that makes it
//! accessible, and assert what the handler saw and that execution resumed with every register
//! intact -- the one property a trampoline that bounces through another thread could get wrong
//! while every counter looked right.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use omni_platform::fault::{self, Fault, FaultAccess, FaultOutcome};
use omni_platform::vm::{self, Protection};

/// One test at a time touches the process-wide table: the counters are process-wide too.
static SERIAL: Mutex<()> = Mutex::new(());

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the resolving handler last saw, for the test to read back.
static SEEN_ADDRESS: AtomicUsize = AtomicUsize::new(0);
static SEEN_ACCESS: AtomicUsize = AtomicUsize::new(0);
static SEEN_THREAD: AtomicU64 = AtomicU64::new(0);
static SEEN_CALLS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// A marker only the faulting thread has: the handler reads it to prove where it ran.
    static MARKER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn access_code(access: FaultAccess) -> usize {
    match access {
        FaultAccess::Read => 1,
        FaultAccess::Write => 2,
        FaultAccess::Execute => 3,
    }
}

/// Resolves a fault inside `[base, base + len)` (the context is the base; the length is one page)
/// by making the page read-write -- or read-execute for a fetch.
fn resolving_handler(base: usize, fault: &Fault) -> FaultOutcome {
    let page = vm::page_size();
    if fault.address < base || fault.address >= base + page {
        return FaultOutcome::NotOurs;
    }
    SEEN_ADDRESS.store(fault.address, Ordering::SeqCst);
    SEEN_ACCESS.store(access_code(fault.access), Ordering::SeqCst);
    SEEN_THREAD.store(MARKER.with(std::cell::Cell::get), Ordering::SeqCst);
    SEEN_CALLS.fetch_add(1, Ordering::SeqCst);
    // Clobber, on purpose, the registers `every_register_survives_a_resolved_fault` checks and the
    // flags: whatever the faulting thread had in them must come back from the saved state, not from
    // the handler happening to leave them alone.
    // SAFETY: only scratch registers of this frame are written, and they are declared clobbered.
    unsafe {
        core::arch::asm!(
            "movz x9, #0xdead", "mov x10, x9", "mov x11, x9", "mov x12, x9", "mov x13, x9",
            "mov x14, x9", "mov x15, x9", "mov x16, x9", "mov x17, x9",
            "movi v7.16b, #0x5a", "cmp x9, x9",
            out("x9") _, out("x10") _, out("x11") _, out("x12") _, out("x13") _, out("x14") _,
            out("x15") _, out("x16") _, out("x17") _, out("v7") _,
        );
    }
    let protection = if fault.access == FaultAccess::Execute {
        Protection::ReadExecute
    } else {
        Protection::ReadWrite
    };
    // SAFETY: the page is inside a reservation the test owns.
    match unsafe { vm::commit(base as *mut u8, page, protection) } {
        Ok(()) => FaultOutcome::Resolved,
        Err(_) => FaultOutcome::NotOurs,
    }
}

/// Run one test of this binary in a child, killing it after a deadline: a defect in the decline
/// or breakpoint path loops for ever rather than failing, and a test harness with no timeout would
/// hang with it.
fn run_child(test: &str, variable: &str) -> (Option<i32>, String) {
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(variable, "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("run the child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            let mut stdout = String::new();
            if let Some(mut pipe) = child.stdout.take() {
                use std::io::Read;
                let _ = pipe.read_to_string(&mut stdout);
            }
            return (status.code(), stdout);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the child running {test} did not finish in 20 s: it is looping on a fault");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// A one-page reservation, not yet accessible.
fn untouched_page() -> vm::Reservation {
    vm::reserve(vm::page_size(), vm::page_size()).expect("a page of address space")
}

#[test]
fn a_fault_is_resolved_on_the_faulting_thread_and_the_access_retried() {
    let _serial = serialized();
    let page = untouched_page();
    // SAFETY: the context is the page's base, which outlives the registration.
    let registration = unsafe { fault::install(resolving_handler, page.base()) }.expect("install");
    let before = fault::stats();
    MARKER.with(|marker| marker.set(0xFA17_0001));
    SEEN_CALLS.store(0, Ordering::SeqCst);
    // SAFETY: the handler makes the page read-write when this store faults on it.
    unsafe { (page.base() as *mut u64).add(3).write_volatile(0xC0FFEE) };
    // SAFETY: as above; the page is now read-write.
    let value = unsafe { (page.base() as *const u64).add(3).read_volatile() };
    assert_eq!(value, 0xC0FFEE, "the store was retried after the fault and landed");
    assert_eq!(SEEN_CALLS.load(Ordering::SeqCst), 1, "one fault, one call");
    assert_eq!(SEEN_ADDRESS.load(Ordering::SeqCst), page.base() + 24, "the exact faulting address");
    assert_eq!(SEEN_ACCESS.load(Ordering::SeqCst), access_code(FaultAccess::Write));
    assert_eq!(
        SEEN_THREAD.load(Ordering::SeqCst),
        0xFA17_0001,
        "the handler saw this thread's thread-local: it ran on the faulting thread"
    );
    let after = fault::stats();
    assert!(after.examined > before.examined && after.resolved > before.resolved);
    drop(registration);
    vm::release(page).expect("release");
}

#[test]
fn a_read_is_reported_as_a_read() {
    let _serial = serialized();
    let page = untouched_page();
    // SAFETY: as above.
    let registration = unsafe { fault::install(resolving_handler, page.base()) }.expect("install");
    // SAFETY: the handler makes the page accessible when this load faults on it.
    let value = unsafe { (page.base() as *const u32).add(1).read_volatile() };
    assert_eq!(value, 0, "a fresh page reads zero");
    assert_eq!(SEEN_ACCESS.load(Ordering::SeqCst), access_code(FaultAccess::Read));
    assert_eq!(SEEN_ADDRESS.load(Ordering::SeqCst), page.base() + 4);
    drop(registration);
    vm::release(page).expect("release");
}

/// Every general register and a vector register survive the round trip through the trampoline:
/// the assembly loads known values, performs the faulting load, and stores the registers back.
#[test]
fn every_register_survives_a_resolved_fault() {
    let _serial = serialized();
    let page = untouched_page();
    // SAFETY: as above.
    let registration = unsafe { fault::install(resolving_handler, page.base()) }.expect("install");
    let mut out = [0u64; 12];
    let mut vec_out = [0u64; 2];
    // SAFETY: the load faults once on the test's own page and is resolved; the registers named
    // are declared clobbered, and `out`/`vec_out` are live buffers of the sizes written.
    unsafe {
        core::arch::asm!(
            "movz x9, #0x1111", "movz x10, #0x2222", "movz x11, #0x3333", "movz x12, #0x4444",
            "movz x13, #0x5555", "movz x14, #0x6666", "movz x15, #0x7777", "movz x16, #0x8888",
            "movz x17, #0x9999", "movz x20, #0xaaaa", "movz x21, #0xbbbb",
            "movz x22, #0xcccc", "fmov d7, x22", "mov v7.d[1], x21",
            "movz x24, #0xa000, lsl #16", "msr nzcv, x24",
            "ldr x25, [{page}]",
            "mrs x23, nzcv",
            "stp x9, x10, [{out}]", "stp x11, x12, [{out}, #16]", "stp x13, x14, [{out}, #32]",
            "stp x15, x16, [{out}, #48]", "stp x17, x20, [{out}, #64]", "stp x21, x23, [{out}, #80]",
            "str q7, [{vec}]",
            page = in(reg) page.base(),
            out = in(reg) out.as_mut_ptr(),
            vec = in(reg) vec_out.as_mut_ptr(),
            out("x9") _, out("x10") _, out("x11") _, out("x12") _, out("x13") _, out("x14") _,
            out("x15") _, out("x16") _, out("x17") _, out("x20") _, out("x21") _,
            out("x22") _, out("x23") _, out("x24") _, out("x25") _, out("v7") _,
        );
    }
    assert_eq!(
        out,
        [0x1111, 0x2222, 0x3333, 0x4444, 0x5555, 0x6666, 0x7777, 0x8888, 0x9999, 0xaaaa, 0xbbbb, 0xa000_0000],
        "general registers and the flags written before the fault read back after it"
    );
    assert_eq!(vec_out, [0xcccc, 0xbbbb], "v7 survived");
    drop(registration);
    vm::release(page).expect("release");
}

/// A thread started **after** the first install is covered too (the introspection hook), and so is
/// one started **before** it and still running (the enumeration). Each faults on its own page.
#[test]
fn threads_started_before_and_after_the_first_install_are_both_covered() {
    let _serial = serialized();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<usize>();
    let early_page = untouched_page();
    let early = std::thread::spawn(move || {
        let base = go_rx.recv().expect("the page to fault on");
        // SAFETY: the handler makes the page accessible when this store faults.
        unsafe { (base as *mut u8).write_volatile(0x5A) };
        // SAFETY: as above.
        unsafe { (base as *const u8).read_volatile() }
    });
    // SAFETY: as above.
    let registration = unsafe { fault::install(resolving_handler, early_page.base()) }.expect("install");
    go_tx.send(early_page.base()).expect("send");
    assert_eq!(early.join().expect("the early thread"), 0x5A);
    drop(registration);

    let late_page = untouched_page();
    // SAFETY: as above.
    let registration = unsafe { fault::install(resolving_handler, late_page.base()) }.expect("install");
    let base = late_page.base();
    let late = std::thread::spawn(move || {
        // SAFETY: as above.
        unsafe { (base as *mut u8).write_volatile(0xA5) };
        // SAFETY: as above.
        unsafe { (base as *const u8).read_volatile() }
    });
    assert_eq!(late.join().expect("the late thread"), 0xA5);
    drop(registration);
    vm::release(early_page).expect("release");
    vm::release(late_page).expect("release");
}

/// A fault no handler owns is passed on -- to the task port and then to the signal -- exactly as if
/// nothing were installed. In a child with a `SIGSEGV`/`SIGBUS` handler that reports and exits, so
/// "passed on" is observed rather than inferred from a crash.
#[test]
fn a_declined_fault_falls_through_to_the_signal() {
    const CHILD: &str = "OMNI_FAULT_MACOS_DECLINE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        extern "C" fn on_signal(signal: i32) {
            // Async-signal-safe: a write and an _exit.
            let message: &[u8] = if signal == 10 { b"SIGBUS\n" } else { b"SIGSEGV\n" };
            // SAFETY: writing a static buffer to stdout, then exiting without unwinding.
            unsafe {
                libc_write(1, message.as_ptr(), message.len());
                libc_exit(42);
            }
        }
        extern "C" {
            #[link_name = "write"]
            fn libc_write(fd: i32, buf: *const u8, len: usize) -> isize;
            #[link_name = "_exit"]
            fn libc_exit(code: i32) -> !;
            fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
        }
        // SAFETY: installing a handler for the two fault signals.
        unsafe {
            signal(10, on_signal);
            signal(11, on_signal);
        }
        let page = untouched_page();
        let other = untouched_page();
        // A handler is installed, and it declines: the fault is not in its page.
        // SAFETY: as above.
        let _registration = unsafe { fault::install(resolving_handler, other.base()) }.expect("install");
        // SAFETY: none -- expected to raise the signal.
        let value = unsafe { (page.base() as *const u8).read_volatile() };
        println!("READ {value}: the fault was not passed on");
        std::process::exit(7);
    }
    let (code, stdout) = run_child("a_declined_fault_falls_through_to_the_signal", CHILD);
    assert_eq!(code, Some(42), "the signal handler ran: {stdout}");
    assert!(stdout.contains("SIGBUS") || stdout.contains("SIGSEGV"), "{stdout}");
}

/// Resolving a fault costs two Mach round trips; measured here so a regression shows as a number.
/// n = 2,000 faults, each on a page decommitted just before.
#[test]
fn the_cost_of_a_resolved_fault_is_measured() {
    let _serial = serialized();
    let page = untouched_page();
    // SAFETY: as above.
    let registration = unsafe { fault::install(resolving_handler, page.base()) }.expect("install");
    let n = 2_000;
    let started = std::time::Instant::now();
    for _ in 0..n {
        // SAFETY: the handler recommits the page on the fault this store takes.
        unsafe { (page.base() as *mut u8).write_volatile(1) };
        // SAFETY: the page is inside the reservation.
        unsafe { vm::decommit(page.as_ptr(), vm::page_size()) }.expect("decommit");
    }
    let per_fault = started.elapsed() / n;
    eprintln!("FAULT COST: {per_fault:?} per resolved fault including a decommit (n = {n})");
    assert!(per_fault < std::time::Duration::from_millis(1), "{per_fault:?} per fault");
    drop(registration);
    vm::release(page).expect("release");
}

/// A fetch from a page with no execute permission is reported as a fetch, and resolving it lets the
/// call proceed: `mov w0, #42; ret` written, the page protected to nothing, then called.
#[test]
fn an_instruction_fetch_is_reported_as_one_and_can_be_resolved() {
    let _serial = serialized();
    extern "C" {
        fn sys_icache_invalidate(start: *mut u8, len: usize);
    }
    let page = untouched_page();
    // SAFETY: the page is the test's own; it is written, then made inaccessible, then called
    // through the handler, which raises it to read-execute.
    unsafe {
        vm::commit(page.as_ptr(), vm::page_size(), Protection::ReadWrite).expect("commit");
        (page.base() as *mut u32).write(0x5280_0540);
        (page.base() as *mut u32).add(1).write(0xd65f_03c0);
        sys_icache_invalidate(page.as_ptr(), 8);
        vm::protect(page.as_ptr(), vm::page_size(), Protection::None).expect("protect none");
    }
    // SAFETY: as above.
    let registration = unsafe { fault::install(resolving_handler, page.base()) }.expect("install");
    // SAFETY: the page holds a complete function; the fetch fault is resolved to read-execute.
    let function: extern "C" fn() -> i32 = unsafe { std::mem::transmute(page.base()) };
    assert_eq!(function(), 42);
    assert_eq!(SEEN_ACCESS.load(Ordering::SeqCst), access_code(FaultAccess::Execute));
    assert_eq!(SEEN_ADDRESS.load(Ordering::SeqCst), page.base(), "the fetch address is the pc");
    drop(registration);
    vm::release(page).expect("release");
}

/// A `brk` that is not the trampoline's is somebody else's -- a debugger's -- and is passed on:
/// with a handler installed, `brk #0` still becomes `SIGTRAP`.
#[test]
fn a_breakpoint_that_is_not_ours_is_passed_on() {
    const CHILD: &str = "OMNI_FAULT_MACOS_BRK_CHILD";
    if std::env::var_os(CHILD).is_some() {
        extern "C" fn on_trap(_signal: i32) {
            // SAFETY: async-signal-safe write and exit.
            unsafe {
                write(1, b"SIGTRAP\n".as_ptr(), 8);
                _exit(43);
            }
        }
        extern "C" {
            fn write(fd: i32, buf: *const u8, len: usize) -> isize;
            fn _exit(code: i32) -> !;
            fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
        }
        let page = untouched_page();
        // SAFETY: as above.
        let _registration = unsafe { fault::install(resolving_handler, page.base()) }.expect("install");
        // SAFETY: installing a handler, then trapping.
        unsafe {
            signal(5, on_trap);
            core::arch::asm!("brk #0");
        }
        println!("the breakpoint was swallowed");
        std::process::exit(7);
    }
    let (code, stdout) = run_child("a_breakpoint_that_is_not_ours_is_passed_on", CHILD);
    assert_eq!(code, Some(43), "SIGTRAP reached the process: {stdout}");
}

static BLOCK_ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static BLOCK_GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Resolves like `resolving_handler`, but first waits at a gate the test opens.
fn gated_handler(base: usize, fault: &Fault) -> FaultOutcome {
    if fault.address < base || fault.address >= base + vm::page_size() {
        return FaultOutcome::NotOurs;
    }
    BLOCK_ENTERED.store(true, Ordering::SeqCst);
    while !BLOCK_GATE.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    resolving_handler(base, fault)
}

/// **Dropping a registration waits for a handler already running on another thread** -- the
/// quiescence the seam promises, observed with a handler held at a gate.
#[test]
fn releasing_a_registration_waits_for_a_handler_in_flight() {
    let _serial = serialized();
    BLOCK_ENTERED.store(false, Ordering::SeqCst);
    BLOCK_GATE.store(false, Ordering::SeqCst);
    let page = untouched_page();
    let base = page.base();
    // SAFETY: as above.
    let registration = unsafe { fault::install(gated_handler, base) }.expect("install");
    let faulting = std::thread::spawn(move || {
        // SAFETY: the gated handler resolves this store once the gate opens.
        unsafe { (base as *mut u8).write_volatile(9) };
    });
    while !BLOCK_ENTERED.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&released);
    let releaser = std::thread::spawn(move || {
        drop(registration);
        flag.store(true, Ordering::SeqCst);
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!released.load(Ordering::SeqCst), "the drop returned while a handler was running");
    BLOCK_GATE.store(true, Ordering::SeqCst);
    releaser.join().expect("the releaser");
    faulting.join().expect("the faulting thread");
    assert!(released.load(Ordering::SeqCst));
    assert!(fault::stats().drained > 0, "the drain was counted");
    vm::release(page).expect("release");
}

/// A thread already running when the **first** install in the process happens is covered: that is
/// the `task_threads` enumeration, not the hook. In a child, because in this binary an earlier test
/// may already have made the first install.
#[test]
fn a_thread_running_before_the_first_install_is_covered() {
    const CHILD: &str = "OMNI_FAULT_MACOS_EARLY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let page = untouched_page();
        let base = page.base();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let early = std::thread::spawn(move || {
            go_rx.recv().expect("go");
            // SAFETY: the handler makes the page accessible when this store faults.
            unsafe { (base as *mut u8).write_volatile(0x77) };
            // SAFETY: as above.
            unsafe { (base as *const u8).read_volatile() }
        });
        // SAFETY: as above. This is the first install in the child process.
        let _registration = unsafe { fault::install(resolving_handler, base) }.expect("install");
        go_tx.send(()).expect("send");
        let value = early.join().expect("the early thread");
        println!("EARLY {value:#x}");
        std::process::exit(if value == 0x77 { 0 } else { 8 });
    }
    let (code, stdout) = run_child("a_thread_running_before_the_first_install_is_covered", CHILD);
    assert_eq!(code, Some(0), "the pre-existing thread's fault was served: {stdout}");
}
