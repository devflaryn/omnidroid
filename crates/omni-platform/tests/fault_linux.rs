//! The Linux guest-fault backend, driven by real faults: what a handler is told about a `SIGSEGV`
//! and a `SIGBUS`, and what it is not handed at all.
//!
//! The chain order -- first place over a handler installed later, and the way back to one installed
//! earlier -- is `fault_chain_linux.rs`, in its own process, because it installs dispositions that
//! must be in place before this module's first `install`.
#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use omni_platform::fault::{self, Fault, FaultAccess, FaultOutcome};
use omni_platform::vm::{self, Protection};

/// The tests share one process-wide dispatch path and assert on its counters.
static SERIAL: Mutex<()> = Mutex::new(());

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

const PAGE: usize = 4096;

/// What the recording handler saw last, and how to fix it.
struct Recorder {
    base: usize,
    len: usize,
    /// The protection to give the faulting page, as a `Protection` index into `Protection::ALL`.
    fix: AtomicUsize,
    address: AtomicUsize,
    access: AtomicUsize,
    instruction_pointer: AtomicUsize,
    calls: AtomicU64,
}

fn access_code(access: FaultAccess) -> usize {
    match access {
        FaultAccess::Read => 1,
        FaultAccess::Write => 2,
        FaultAccess::Execute => 3,
    }
}

fn record(context: usize, fault: &Fault) -> FaultOutcome {
    // SAFETY: `context` is a leaked `Recorder`, valid for the whole process.
    let recorder: &Recorder = unsafe { &*(context as *const Recorder) };
    if fault.address < recorder.base || fault.address >= recorder.base + recorder.len {
        return FaultOutcome::NotOurs;
    }
    recorder.calls.fetch_add(1, Ordering::Relaxed);
    recorder.address.store(fault.address, Ordering::Relaxed);
    recorder.access.store(access_code(fault.access), Ordering::Relaxed);
    recorder.instruction_pointer.store(fault.instruction_pointer, Ordering::Relaxed);
    let page = fault.address & !(PAGE - 1);
    let protection = Protection::ALL[recorder.fix.load(Ordering::Relaxed)];
    // SAFETY: the page lies inside a live plain reservation the test owns until the handler is gone.
    match unsafe { vm::commit(page as *mut u8, PAGE, protection) } {
        Ok(()) => FaultOutcome::Resolved,
        Err(_) => FaultOutcome::NotOurs,
    }
}

fn leak_recorder(base: usize, len: usize) -> &'static Recorder {
    Box::leak(Box::new(Recorder {
        base,
        len,
        fix: AtomicUsize::new(2),
        address: AtomicUsize::new(0),
        access: AtomicUsize::new(0),
        instruction_pointer: AtomicUsize::new(0),
        calls: AtomicU64::new(0),
    }))
}

fn protection_index(protection: Protection) -> usize {
    Protection::ALL.iter().position(|&p| p == protection).expect("a protection")
}

/// **A read, a write and an instruction fetch are told apart**, from the page-fault error code
/// (`REG_ERR`: bit 1 write, bit 4 instruction fetch), and each arrives with the exact faulting
/// address. The fetch also names its instruction pointer, which for a fetch *is* the address.
#[test]
fn a_read_a_write_and_a_fetch_are_each_reported_as_what_they_are() {
    let _serial = serialized();
    let reservation = vm::reserve(4 * PAGE, PAGE).expect("reserve");
    let base = reservation.base();
    let recorder = leak_recorder(base, 4 * PAGE);
    // SAFETY: the recorder is leaked, its handler cannot unwind, takes only the vm ledger, and
    // resolves only after committing the page.
    let registration = unsafe { fault::install(record, recorder as *const Recorder as usize) }
        .expect("install");
    let before = fault::stats();

    // A read of an uncommitted page, one byte into it.
    recorder.fix.store(protection_index(Protection::ReadWrite), Ordering::Relaxed);
    // SAFETY: inside the live reservation; the handler commits it.
    let value = unsafe { core::ptr::read_volatile((base + 7) as *const u8) };
    assert_eq!(value, 0, "a freshly committed page reads zero");
    assert_eq!(recorder.address.load(Ordering::Relaxed), base + 7, "the exact address");
    assert_eq!(recorder.access.load(Ordering::Relaxed), access_code(FaultAccess::Read));

    // A write to another.
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile((base + PAGE + 9) as *mut u8, 0x42) };
    assert_eq!(recorder.address.load(Ordering::Relaxed), base + PAGE + 9);
    assert_eq!(recorder.access.load(Ordering::Relaxed), access_code(FaultAccess::Write));
    // SAFETY: committed now.
    assert_eq!(unsafe { core::ptr::read_volatile((base + PAGE + 9) as *const u8) }, 0x42);

    // A write to a page that is committed read-only is a *protection* fault (SEGV_ACCERR), and is
    // reported as a write too.
    // SAFETY: inside the reservation.
    unsafe { vm::commit((base + 2 * PAGE) as *mut u8, PAGE, Protection::Read) }.expect("commit r");
    // SAFETY: as above; the handler raises it to read-write.
    unsafe { core::ptr::write_volatile((base + 2 * PAGE) as *mut u8, 0x43) };
    assert_eq!(recorder.access.load(Ordering::Relaxed), access_code(FaultAccess::Write));

    // An instruction fetch: `ret` in a readable, non-executable page, called. The handler makes it
    // r-x, and the call returns.
    let code = base + 3 * PAGE;
    // SAFETY: inside the reservation; written while read-write, then dropped to read-only.
    unsafe {
        vm::commit(code as *mut u8, PAGE, Protection::ReadWrite).expect("commit rw");
        core::ptr::write_volatile(code as *mut u8, 0xC3);
        vm::protect(code as *mut u8, PAGE, Protection::Read).expect("drop to read-only");
    }
    recorder.fix.store(protection_index(Protection::ReadExecute), Ordering::Relaxed);
    // SAFETY: the page holds a single `ret`; once executable, calling it returns immediately.
    let function: extern "C" fn() = unsafe { core::mem::transmute(code) };
    function();
    assert_eq!(recorder.access.load(Ordering::Relaxed), access_code(FaultAccess::Execute));
    assert_eq!(recorder.address.load(Ordering::Relaxed), code);
    assert_eq!(
        recorder.instruction_pointer.load(Ordering::Relaxed),
        code,
        "for a fetch the faulting instruction is the address itself"
    );

    let after = fault::stats();
    assert_eq!(recorder.calls.load(Ordering::Relaxed), 4, "four faults, one call each");
    assert!(after.resolved - before.resolved >= 4, "{before:?} -> {after:?}");
    drop(registration);
    vm::release(reservation).expect("release");
}

/// **`SIGBUS` is a guest fault too**: a file-backed page wholly past the end of its file. The handler
/// is told, and here resolves it by putting anonymous memory over the page.
#[test]
fn a_page_past_the_end_of_a_file_arrives_as_a_fault_on_sigbus() {
    let _serial = serialized();
    // A two-page file, mapped whole, then truncated to one page: the view's second page now lies
    // wholly past the end of the file, and touching it is SIGBUS (BUS_ADRERR).
    let dir = std::env::temp_dir().join("omnidroid-vm-tests");
    std::fs::create_dir_all(&dir).expect("fixture directory");
    let path = dir.join(format!("sigbus-{}.bin", std::process::id()));
    std::fs::write(&path, vec![0x77u8; 2 * PAGE]).expect("write the fixture");
    let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).expect("open");
    let shared = vm::share_file_for_mapping(file, &path).expect("share");
    let view = vm::reserve_placeholder(2 * PAGE, PAGE).expect("reserve");
    // SAFETY: the reservation is one exact-size placeholder.
    unsafe { vm::map_file(&shared, 0, 2 * PAGE, view.as_ptr(), Protection::Read) }.expect("map");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_len(PAGE as u64))
        .expect("truncate to one page");

    static BUS_CALLS: AtomicU64 = AtomicU64::new(0);
    static BUS_ADDRESS: AtomicUsize = AtomicUsize::new(0);
    static BUS_TARGET: AtomicUsize = AtomicUsize::new(0);
    fn on_bus(_context: usize, fault: &Fault) -> FaultOutcome {
        if fault.address & !(PAGE - 1) != BUS_TARGET.load(Ordering::Relaxed) {
            return FaultOutcome::NotOurs;
        }
        BUS_CALLS.fetch_add(1, Ordering::Relaxed);
        BUS_ADDRESS.store(fault.address, Ordering::Relaxed);
        // Put a zero page over it. Deliberately the raw primitive, not the seam: the seam would
        // (rightly) refuse to commit over a view.
        // SAFETY: the page is the second page of this test's own view.
        let mapped = unsafe {
            libc::mmap(
                (fault.address & !(PAGE - 1)) as *mut libc::c_void,
                PAGE,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            FaultOutcome::NotOurs
        } else {
            FaultOutcome::Resolved
        }
    }
    let target = view.base() + PAGE;
    BUS_TARGET.store(target, Ordering::Relaxed);
    // SAFETY: `on_bus` touches only statics, cannot unwind, and resolves only after remapping.
    let registration = unsafe { fault::install(on_bus, 0) }.expect("install");
    // SAFETY: page 0 is inside the file; page 1 is past it, and the handler fixes it.
    let (inside, past) = unsafe {
        (
            core::ptr::read_volatile(view.as_ptr()),
            core::ptr::read_volatile((target + 5) as *const u8),
        )
    };
    assert_eq!(inside, 0x77);
    assert_eq!(past, 0);
    assert_eq!(BUS_CALLS.load(Ordering::Relaxed), 1, "one SIGBUS, reported once");
    assert_eq!(BUS_ADDRESS.load(Ordering::Relaxed), target + 5);
    drop(registration);
    // SAFETY: the whole view, which nothing refers to any more. Its second page was replaced
    // behind the seam's back above, which munmap does not mind.
    unsafe { vm::unmap_and_release(view.as_ptr(), 2 * PAGE) }.expect("unmap");
    let _ = std::fs::remove_file(&path);
}

/// **A signal that was *sent* is not a fault**, and is never shown to a handler: `si_code <= 0`
/// means `kill`/`raise`, whose `si_addr` is meaningless. It is passed on down the chain instead.
/// Checked with a handler that would claim anything, and a `raise(SIGBUS)` whose disposition
/// further down is a counting handler installed before this module was.
///
/// Run in a child process, because the disposition it installs first must be the *original* one,
/// and in this binary the other tests may already have installed this module.
#[test]
fn a_sent_signal_is_never_shown_to_a_handler() {
    let output = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--ignored", "--exact", "child_a_sent_signal_is_passed_down_the_chain", "--nocapture"])
        .output()
        .expect("run the child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the child failed: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed"), "the child test did not run: {stdout}");
}

static SENT_SEEN_BELOW: AtomicU64 = AtomicU64::new(0);
static CLAIMED: AtomicU64 = AtomicU64::new(0);
static CHILD_REGION: AtomicUsize = AtomicUsize::new(0);

extern "C" fn below(_signal: libc::c_int, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    // SAFETY: the kernel's siginfo.
    if unsafe { (*info).si_code } <= 0 {
        SENT_SEEN_BELOW.fetch_add(1, Ordering::Relaxed);
    }
}

/// Serves its one page for real, and "resolves" anything else it is shown while counting it -- so a
/// sent signal that reached it would be visible, and would not kill the child.
fn claim_everything(_context: usize, fault: &Fault) -> FaultOutcome {
    let region = CHILD_REGION.load(Ordering::Relaxed);
    if fault.address & !(PAGE - 1) == region {
        // SAFETY: the child's own plain reservation.
        return match unsafe { vm::commit(region as *mut u8, PAGE, Protection::ReadWrite) } {
            Ok(()) => FaultOutcome::Resolved,
            Err(_) => FaultOutcome::NotOurs,
        };
    }
    CLAIMED.fetch_add(1, Ordering::Relaxed);
    FaultOutcome::Resolved
}

#[test]
#[ignore = "run by a_sent_signal_is_never_shown_to_a_handler in a child process"]
fn child_a_sent_signal_is_passed_down_the_chain() {
    // SAFETY: plain data; an SA_SIGINFO action for `below`, which touches only a static.
    unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        action.sa_sigaction = below as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut action.sa_mask);
        assert_eq!(libc::sigaction(libc::SIGBUS, &action, core::ptr::null_mut()), 0);
    }
    // SAFETY: the handler touches only statics and the vm ledger, and cannot unwind.
    let registration = unsafe { fault::install(claim_everything, 0) }.expect("install");
    // One real fault first, on this thread. The kernel reports the *thread's last trap* in every
    // signal frame, sent signals included, so after this the frame of the `raise` below says "page
    // fault" (vector 14) -- only `si_code` tells the two apart, which is what this test pins.
    let region = vm::reserve(PAGE, PAGE).expect("reserve");
    CHILD_REGION.store(region.base(), Ordering::Relaxed);
    // SAFETY: inside the reservation; the handler commits it.
    unsafe { core::ptr::write_volatile(region.as_ptr(), 1) };
    assert_eq!(CLAIMED.load(Ordering::Relaxed), 0);
    // SAFETY: raise is always safe to call; SIGBUS is handled above.
    assert_eq!(unsafe { libc::raise(libc::SIGBUS) }, 0);
    assert_eq!(CLAIMED.load(Ordering::Relaxed), 0, "a sent signal was dispatched as a fault");
    assert_eq!(SENT_SEEN_BELOW.load(Ordering::Relaxed), 1, "and it must reach the handler below");
    drop(registration);
}
