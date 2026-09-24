//! **First place, and the way down the chain**: the ordering D4 and D10 need on Linux, where a
//! signal has one disposition and the last `sigaction` wins.
//!
//! Three handlers, in the order a real process gets them:
//!
//! * **EARLIER** -- installed before this module, as Rust's own stack-overflow handler is. It owns
//!   region P.
//! * **Omnidroid's** -- `fault::install`, owning region A (the demand pager's role).
//! * **LATER** -- installed after, *exactly as dynarmic's POSIX handler is*: it saves the disposition
//!   it displaced, takes any fault it considers its own **without passing it on**, and forwards the
//!   rest to what it saved. It claims region A too, which stands for "a fault at a RIP inside my
//!   code cache": that is the fault dynarmic answers with its fastmem fallback, and the one D10 says
//!   must reach the pager first.
//!
//! The run shows the problem (LATER on top takes region A's fault), the fix
//! (`fault::reassert_precedence` puts Omnidroid's handler back on top), and that the fix does not
//! loop: a fault nobody above EARLIER owns goes Omnidroid -> LATER -> (LATER's saved = Omnidroid,
//! re-entered while chaining) -> EARLIER, and is served there exactly once.
//!
//! One `#[test]` drives it in sequence, in its own binary, because every step changes process-wide
//! dispositions the next step depends on -- and EARLIER must be installed before this module's
//! first `install` in the process, which no other test here may do first.
#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use omni_platform::fault::{self, Fault, FaultOutcome};
use omni_platform::vm::{self, Protection};

const PAGE: usize = 4096;

/// Region bounds, published before the handler that reads them is installed.
static P_BASE: AtomicUsize = AtomicUsize::new(0);
static A_BASE: AtomicUsize = AtomicUsize::new(0);
static L_BASE: AtomicUsize = AtomicUsize::new(0);
const REGION: usize = 16 * PAGE;

static EARLIER_SERVED: AtomicU64 = AtomicU64::new(0);
static OURS_SERVED: AtomicU64 = AtomicU64::new(0);
static OURS_ENTERED: AtomicU64 = AtomicU64::new(0);
static LATER_SERVED: AtomicU64 = AtomicU64::new(0);
static LATER_ENTERED: AtomicU64 = AtomicU64::new(0);

/// LATER's saved disposition, written once before LATER is installed.
static mut LATER_OLD: core::mem::MaybeUninit<libc::sigaction> = core::mem::MaybeUninit::uninit();

fn in_region(address: usize, base: &AtomicUsize) -> bool {
    let base = base.load(Ordering::Relaxed);
    base != 0 && address >= base && address < base + REGION
}

/// Make the page at `address` readable and writable with the raw primitive, as a foreign handler
/// (which knows nothing of the seam's ledger) would.
fn fix_raw(address: usize) -> bool {
    // SAFETY: the page lies in a reservation this test owns for the life of the process.
    unsafe {
        libc::mprotect(
            (address & !(PAGE - 1)) as *mut libc::c_void,
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
        ) == 0
    }
}

fn fault_address(info: *mut libc::siginfo_t) -> usize {
    // SAFETY: the kernel's siginfo for this delivery.
    unsafe { (*info).si_addr() as usize }
}

extern "C" fn earlier(_signal: libc::c_int, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    let address = fault_address(info);
    if in_region(address, &P_BASE) && fix_raw(address) {
        EARLIER_SERVED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // Not ours and nothing below us: the default action, at the real fault.
    // SAFETY: resetting a disposition is async-signal-safe.
    unsafe { libc::signal(libc::SIGSEGV, libc::SIG_DFL) };
}

/// dynarmic's `SigHandler::SigAction`, reduced to what matters: claim what is "mine" and never
/// chain it; forward everything else to the saved disposition, `SA_SIGINFO` style.
extern "C" fn later(signal: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    LATER_ENTERED.fetch_add(1, Ordering::Relaxed);
    let address = fault_address(info);
    if (in_region(address, &L_BASE) || in_region(address, &A_BASE)) && fix_raw(address) {
        LATER_SERVED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: written once before `later` was installed, never again.
    let old = unsafe { (*core::ptr::addr_of!(LATER_OLD)).assume_init_ref() };
    if old.sa_flags & libc::SA_SIGINFO != 0 {
        // SAFETY: an SA_SIGINFO action has this signature.
        let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
            unsafe { core::mem::transmute(old.sa_sigaction) };
        f(signal, info, context);
    } else {
        // SAFETY: as `earlier`.
        unsafe { libc::signal(signal, libc::SIG_DFL) };
    }
}

fn ours(_context: usize, fault: &Fault) -> FaultOutcome {
    OURS_ENTERED.fetch_add(1, Ordering::Relaxed);
    if !in_region(fault.address, &A_BASE) {
        return FaultOutcome::NotOurs;
    }
    let page = fault.address & !(PAGE - 1);
    // SAFETY: the page lies inside this test's plain reservation.
    match unsafe { vm::commit(page as *mut u8, PAGE, Protection::ReadWrite) } {
        Ok(()) => {
            OURS_SERVED.fetch_add(1, Ordering::Relaxed);
            FaultOutcome::Resolved
        }
        Err(_) => FaultOutcome::NotOurs,
    }
}

fn install_sigaction(handler: usize, old: Option<*mut libc::sigaction>) {
    // SAFETY: plain data; an SA_SIGINFO | SA_ONSTACK action for a handler of that signature.
    unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        action.sa_sigaction = handler;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        libc::sigemptyset(&mut action.sa_mask);
        let old = old.unwrap_or(core::ptr::null_mut());
        assert_eq!(libc::sigaction(libc::SIGSEGV, &action, old), 0, "sigaction");
    }
}

fn current_handler() -> usize {
    // SAFETY: plain data, filled in by a query-only sigaction.
    unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        assert_eq!(libc::sigaction(libc::SIGSEGV, core::ptr::null(), &mut action), 0);
        action.sa_sigaction
    }
}

/// Touch the next untouched page of a region: one fault each time.
fn touch(base: &AtomicUsize, page: usize) -> u8 {
    let address = base.load(Ordering::Relaxed) + page * PAGE + 3;
    // SAFETY: the address is inside a live reservation this test owns; whichever handler owns the
    // region makes it accessible, and if none does the process dies, which is the honest outcome.
    unsafe {
        core::ptr::write_volatile(address as *mut u8, 0x21);
        core::ptr::read_volatile(address as *const u8)
    }
}

fn counts() -> [u64; 5] {
    [
        EARLIER_SERVED.load(Ordering::Relaxed),
        OURS_ENTERED.load(Ordering::Relaxed),
        OURS_SERVED.load(Ordering::Relaxed),
        LATER_ENTERED.load(Ordering::Relaxed),
        LATER_SERVED.load(Ordering::Relaxed),
    ]
}

fn delta(before: [u64; 5]) -> [u64; 5] {
    let now = counts();
    core::array::from_fn(|i| now[i] - before[i])
}

#[test]
fn omnidroid_is_first_over_a_later_handler_and_the_chain_still_reaches_an_earlier_one() {
    // Three plain reservations, never released: the handlers read their bounds for the life of the
    // process. Made with the seam so that `ours` may commit into A through it.
    let p = vm::reserve(REGION, PAGE).expect("region P");
    let a = vm::reserve(REGION, PAGE).expect("region A");
    let l = vm::reserve(REGION, PAGE).expect("region L");
    P_BASE.store(p.base(), Ordering::Relaxed);
    A_BASE.store(a.base(), Ordering::Relaxed);
    L_BASE.store(l.base(), Ordering::Relaxed);

    // 0. EARLIER, before this module has installed anything in the process.
    install_sigaction(earlier as *const () as usize, None);
    // Reasserting before anything is installed is a no-op, not an install.
    fault::reassert_precedence().expect("a no-op");
    assert_eq!(current_handler(), earlier as *const () as usize, "reassert_precedence must not install");

    // 1. Omnidroid's handler, on top of EARLIER.
    // SAFETY: `ours` touches statics and the vm ledger only, cannot unwind, and resolves only after
    // committing the page.
    let registration = unsafe { fault::install(ours, 1) }.expect("install");
    assert_ne!(current_handler(), earlier as *const () as usize, "fault::install must take first place");
    let ours_address = current_handler();

    // 2. A fault in A is ours.
    let before = counts();
    assert_eq!(touch(&A_BASE, 0), 0x21);
    assert_eq!(delta(before), [0, 1, 1, 0, 0], "A with Omnidroid on top");

    // 3. A fault in P is declined and forwarded to EARLIER, once.
    let before = counts();
    assert_eq!(touch(&P_BASE, 0), 0x21);
    assert_eq!(delta(before), [1, 1, 0, 0, 0], "P forwarded to the earlier handler");

    // 4. LATER installs itself on top, saving Omnidroid's handler, as dynarmic does at its first
    //    jit. From here the problem is live: a fault in A is taken by LATER and never reaches us --
    //    for dynarmic, the fastmem fallback and the 30-49x path.
    // SAFETY: writes the static once, before `later` can run.
    install_sigaction(later as *const () as usize, Some(unsafe { (*core::ptr::addr_of_mut!(LATER_OLD)).as_mut_ptr() }));
    let before = counts();
    assert_eq!(touch(&A_BASE, 1), 0x21);
    assert_eq!(
        delta(before),
        [0, 0, 0, 1, 1],
        "with LATER on top, A's fault is LATER's: this is the ordering problem reproduced"
    );

    // 5. The fix.
    fault::reassert_precedence().expect("reassert first place");
    assert_eq!(current_handler(), ours_address, "Omnidroid's handler is on top again");

    // 6. A fault in A is ours again, and LATER never sees it.
    let before = counts();
    assert_eq!(touch(&A_BASE, 2), 0x21);
    assert_eq!(delta(before), [0, 1, 1, 0, 0], "A after the re-assertion");

    // 7. A fault in L is declined by us and served by LATER, which is now our next link.
    let before = counts();
    assert_eq!(touch(&L_BASE, 0), 0x21);
    assert_eq!(delta(before), [0, 1, 0, 1, 1], "L forwarded to the later handler");

    // 8. The loop. P belongs to neither of the top two: Omnidroid declines and forwards to LATER,
    //    LATER forwards to *its* saved disposition, which is Omnidroid's handler -- entered again
    //    while chaining, so it must not dispatch a second time but go straight to EARLIER. Omnidroid
    //    is entered exactly once and EARLIER serves it exactly once; a loop would never return.
    let before = counts();
    assert_eq!(touch(&P_BASE, 1), 0x21);
    assert_eq!(delta(before), [1, 1, 0, 1, 0], "P through both, without a loop");

    // 9. Idempotent: a second re-assertion with nothing on top changes nothing.
    fault::reassert_precedence().expect("reassert again");
    assert_eq!(current_handler(), ours_address);
    let before = counts();
    assert_eq!(touch(&A_BASE, 3), 0x21);
    assert_eq!(delta(before), [0, 1, 1, 0, 0], "A after an idempotent re-assertion");
    // And the next link is still LATER: a re-assertion that found itself on top must not have
    // recorded itself as the handler to forward to.
    let before = counts();
    assert_eq!(touch(&L_BASE, 1), 0x21);
    assert_eq!(delta(before), [0, 1, 0, 1, 1], "L after an idempotent re-assertion");

    drop(registration);
    println!(
        "chain: EARLIER served {}, Omnidroid entered {} / served {}, LATER entered {} / served {} \
         (n = 1 fault per step, 8 steps, deterministic counters)",
        counts()[0],
        counts()[1],
        counts()[2],
        counts()[3],
        counts()[4]
    );
}

/// Run `child` (an ignored test in this binary) and return how it ended, failing if it neither
/// died nor finished within the deadline -- which is what a chain that returns into the same fault
/// for ever looks like from outside.
fn run_child(child: &str) -> std::process::ExitStatus {
    let mut process = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--ignored", "--exact", child, "--nocapture"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(status) = process.try_wait().expect("wait") {
            return status;
        }
        if std::time::Instant::now() > deadline {
            let _ = process.kill();
            panic!("{child} neither died nor finished within 20 s: the chain loops");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// **An unresolved fault still kills the process, by `SIGSEGV`, at the fault** -- through the whole
/// three-handler chain, ending in Rust's own handler, which resets to `SIG_DFL`.
#[test]
fn a_fault_nobody_owns_ends_the_process_with_sigsegv() {
    use std::os::unix::process::ExitStatusExt;
    let status = run_child("child_an_unowned_fault");
    assert_eq!(status.signal(), Some(libc::SIGSEGV), "expected death by SIGSEGV; got {status}");
}

/// And with **`SIG_DFL` itself** below this module rather than Rust's handler: the forward must
/// reset the disposition, so that the re-executed instruction takes the default action. A forward
/// that returned without resetting would put the thread back into this handler for ever.
#[test]
fn a_fault_nobody_owns_ends_the_process_when_the_disposition_below_is_sig_dfl() {
    use std::os::unix::process::ExitStatusExt;
    let status = run_child("child_an_unowned_fault_over_sig_dfl");
    assert_eq!(status.signal(), Some(libc::SIGSEGV), "expected death by SIGSEGV; got {status}");
}

fn nothing(_context: usize, _fault: &Fault) -> FaultOutcome {
    FaultOutcome::NotOurs
}

fn fault_on_an_inaccessible_page() -> u8 {
    let reservation = vm::reserve(PAGE, PAGE).expect("reserve");
    // SAFETY: deliberately not safe: a read of an inaccessible page that nobody will make
    // accessible. The process is meant to die here.
    unsafe { core::ptr::read_volatile(reservation.as_ptr()) }
}

#[test]
#[ignore = "run by a_fault_nobody_owns_ends_the_process_with_sigsegv in a child process"]
fn child_an_unowned_fault() {
    // SAFETY: `nothing` declines everything and touches nothing.
    let _registration = unsafe { fault::install(nothing, 0) }.expect("install");
    // SAFETY: writes the static once, before `later` can run.
    install_sigaction(later as *const () as usize, Some(unsafe { (*core::ptr::addr_of_mut!(LATER_OLD)).as_mut_ptr() }));
    fault::reassert_precedence().expect("reassert");
    let value = fault_on_an_inaccessible_page();
    println!("unreachable: read {value}");
}

#[test]
#[ignore = "run by a_fault_nobody_owns_ends_the_process_when_the_disposition_below_is_sig_dfl"]
fn child_an_unowned_fault_over_sig_dfl() {
    // SAFETY: resetting a disposition to the default is always sound.
    unsafe { libc::signal(libc::SIGSEGV, libc::SIG_DFL) };
    // SAFETY: `nothing` declines everything and touches nothing.
    let _registration = unsafe { fault::install(nothing, 0) }.expect("install");
    let value = fault_on_an_inaccessible_page();
    println!("unreachable: read {value}");
}
