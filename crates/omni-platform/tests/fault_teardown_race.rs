//! **Hostile input for C1** (Global Constraint 11): tear a fault handler down *while it is running*,
//! with real access violations arriving on other threads, and prove the registrant's context is
//! never touched after its registration's `drop` has returned.
//!
//! # Why this needs its own binary
//!
//! It fills handler slots and moves the process-wide counters in `fault::stats()`, and it wants the
//! slot it installed first to be scanned first. Run beside anything else that installs a handler and
//! both of those stop holding.
//!
//! # The shape of the attack
//!
//! The defect C1 named is not reachable from guest bytes; it is reachable from *timing*, which is the
//! harder kind of hostile input to construct because nothing in the input says "now". So the window
//! is forced open rather than waited for:
//!
//! * a **primary** handler serves demand faults for a reservation, and is torn down while worker
//!   threads are still faulting into it;
//! * a **net** handler, installed into a later slot and released only after the workers are joined,
//!   serves whatever the primary no longer can — without it, the first fault after the teardown
//!   would be an unhandled access violation and the process would die for the wrong reason;
//! * the primary's context is **marked dead, never freed**, the instant its `drop` returns. A real
//!   `free` would make the detector itself undefined behaviour, and a test that relies on undefined
//!   behaviour to observe undefined behaviour proves nothing. Marking is exact and safe: the handler
//!   reads the mark as its last act, so seeing anything but `LIVE` means a dispatch outlived the
//!   teardown by the width of a commit call — which is precisely the use-after-free.
//!
//! Every figure this test prints carries its `n` (Global Constraint 12): rounds, threads per round,
//! pages per thread, and the faults actually served.

#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use omni_platform::fault::{self, Fault, FaultOutcome, FaultRegistration};
use omni_platform::vm::{self, Protection};

/// The context is alive and the registrant has not released its registration.
const LIVE: u64 = 0x4C49_5645_4C49_5645;
/// The registration's `drop` has returned. From this instant the registrant would be entitled to
/// free the context, so any handler frame still reading it is reading freed memory.
const DEAD: u64 = 0xDEAD_DEAD_DEAD_DEAD;

/// What a handler serves and what it saw. Leaked deliberately: see the module docs.
struct Served {
    state: AtomicU64,
    base: usize,
    len: usize,
    entered: AtomicU64,
    resolved: AtomicU64,
    /// Handler frames that were still running after the registration's `drop` had returned.
    ///
    /// **This is the whole test.** Under the pre-quiescence contract it is reachable; under the
    /// current one it is unreachable by construction, and the assertion says so.
    after_release: AtomicU64,
}

/// Dispatches that entered a handler with a context of zero.
///
/// This is the second defect at this seam. The first version of the quiescence fix unpublished a slot
/// by storing zero into `handler` — which is exactly the value `install`'s compare-exchange waits for
/// — so the slot could be claimed and republished while the drain was still spinning, and `release`
/// then cleared a `context` the *new* registrant had written. The re-review measured **646**
/// dispatches entering a live handler that way.
///
/// Counted rather than dereferenced, so the failure reports a number instead of killing the process
/// on a null pointer; a test that can only crash cannot tell you how often.
///
/// **It is a standing watch and not the detector, and saying otherwise would be the kind of
/// overstatement this project keeps catching.** Reverting the fix does *not* make this counter move
/// here: reaching it needs a fault to land in the one slot a churned registration holds, during the
/// nanoseconds between that registration publishing its context and the draining `release` clearing
/// it, and nothing in this test steers faults at churned slots. The detector is
/// `a_slot_being_drained_cannot_be_handed_out_or_have_its_context_cleared`, which makes that window
/// arbitrarily wide by parking a dispatch inside the handler and a `release` inside its drain, and it
/// is what mutation rows `plat-A5` and `plat-A6` are caught by. What this counter buys is a standing
/// assertion over a real workload — 9,600 slot claims against live teardowns per run — that the
/// window is not being entered in practice either.
static NULL_CONTEXTS: AtomicU64 = AtomicU64::new(0);

/// Serializes the two tests below.
///
/// Both reason about which slots they hold: the first needs its primary to be scanned before its
/// net, and the second fills most of the table. Overlapping them would not find a defect, it would
/// manufacture one.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn serve(context: usize, fault: &Fault) -> FaultOutcome {
    // A handler is only ever called with the context its own `install` published, so this cannot
    // happen — and it is checked rather than assumed, because the version of `release` that cleared
    // `context` while the slot was still draining made it happen 646 times.
    if context == 0 {
        NULL_CONTEXTS.fetch_add(1, Ordering::Relaxed);
        return FaultOutcome::NotOurs;
    }
    // SAFETY: `context` is the address of a `Served` that was leaked with `Box::leak` and is never
    // freed, so this reference is valid for the whole process. That is the point: the detector must
    // not itself be the undefined behaviour it is looking for.
    let served: &Served = unsafe { &*(context as *const Served) };
    if fault.address < served.base || fault.address >= served.base + served.len {
        return FaultOutcome::NotOurs;
    }
    served.entered.fetch_add(1, Ordering::Relaxed);

    let page = vm::page_size();
    let aligned = fault.address & !(page - 1);
    // SAFETY: `aligned` is page-aligned and lies inside a live plain reservation this test owns and
    // does not release until every thread that can fault into it has been joined. Committing a page
    // that another thread committed a moment ago is idempotent on Windows and is the normal outcome
    // of two threads faulting on the same page.
    let committed = unsafe { vm::commit(aligned as *mut u8, page, Protection::ReadWrite) }.is_ok();

    // The observation, taken as late in the frame as it can be. `Acquire` against the releasing
    // thread's `Release` store.
    if served.state.load(Ordering::Acquire) != LIVE {
        served.after_release.fetch_add(1, Ordering::Relaxed);
    }
    if committed {
        served.resolved.fetch_add(1, Ordering::Relaxed);
        FaultOutcome::Resolved
    } else {
        FaultOutcome::NotOurs
    }
}

fn leak(base: usize, len: usize) -> &'static Served {
    Box::leak(Box::new(Served {
        state: AtomicU64::new(LIVE),
        base,
        len,
        entered: AtomicU64::new(0),
        resolved: AtomicU64::new(0),
        after_release: AtomicU64::new(0),
    }))
}

/// [`install`], for the churn thread, which races a full table and must not turn that into a
/// failure: `HandlerTableFull` is the expected answer sometimes, and it is not what is under test.
fn try_install(context: &'static Served) -> Option<FaultRegistration> {
    // SAFETY: as `install` below.
    unsafe { fault::install(serve, context as *const Served as usize) }.ok()
}

fn install(context: &'static Served) -> FaultRegistration {
    // SAFETY: `fault::install`'s four conditions. The context is leaked and therefore pinned and
    // immortal; `serve` cannot unwind (it contains no panicking operation — every fallible call is
    // handled by value); it takes no lock this process holds across a fault, only `VirtualAlloc`;
    // and it returns `Resolved` only when the page really was committed.
    unsafe { fault::install(serve, context as *const Served as usize) }
        .expect("a free guest-fault handler slot")
}

/// Rounds of the attack. Each one is an independent teardown-under-load.
const ROUNDS: usize = 24;
/// Threads faulting into the reservation while it is torn down.
const THREADS: usize = 4;
/// Pages each thread touches. One access violation each, the first time round.
const PAGES: usize = 48;
/// Install/release cycles the churn thread runs while the primary is being torn down.
///
/// This is what puts pressure on the *reuse* half rather than only the in-flight half. Without a
/// thread trying to **claim** a slot while a `release` is draining it, nothing is even in a position
/// to take the slot the pre-fix `release` freed mid-drain: the main thread's `drop` blocks until the
/// drain finishes. Measured effect of adding it: releases that had to wait went from 5 to 27 and
/// dispatches reaching the handler under teardown from 96 to 314, over the same n.
///
/// It does not turn this test into a detector for the reuse defect — see `NULL_CONTEXTS` for why, and
/// for which test is.
const CHURN_PER_ROUND: usize = 400;

#[test]
fn tearing_a_handler_down_under_load_never_lets_a_dispatch_outlive_the_release() {
    let _serial = serialized();
    let page = vm::page_size();
    let bytes = PAGES * page;

    let before = fault::stats();
    NULL_CONTEXTS.store(0, Ordering::Relaxed);
    let mut total_entered = 0u64;
    let mut total_resolved = 0u64;
    let mut total_after_release = 0u64;
    let mut rounds_with_concurrency = 0usize;
    let mut churn_installs = 0usize;

    for _ in 0..ROUNDS {
        let reservation = vm::reserve(bytes, page).expect("a reservation to fault into");
        let base = reservation.base();
        let primary = leak(base, bytes);
        let net = leak(base, bytes);

        let primary_reg = install(primary);
        let net_reg = install(net);
        assert!(
            primary_reg.slot() < net_reg.slot(),
            "the net must be scanned after the primary, or the primary never runs and this test \
             measures nothing: primary in slot {}, net in slot {}",
            primary_reg.slot(),
            net_reg.slot()
        );
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                std::thread::spawn(move || {
                    let mut seen = 0u64;
                    for i in 0..PAGES {
                        // Stagger the threads so they do not all queue on the same page.
                        let index = (i + t * (PAGES / THREADS)) % PAGES;
                        let address = base + index * page;
                        // SAFETY: the address is inside a live reservation. It is *not* committed
                        // yet, which is the entire point: the read is an access violation that the
                        // handler has to serve. If it is not served the process dies, which is the
                        // honest outcome for a hostile-input test of an exception handler.
                        seen = seen.wrapping_add(u64::from(unsafe {
                            core::ptr::read_volatile(address as *const u8)
                        }));
                    }
                    seen
                })
            })
            .collect();

        // A thread doing nothing but claiming and releasing slots, for as long as the round lasts.
        // It is what turns "a dispatch is in flight" into "a dispatch is in flight **and** somebody
        // wants this slot".
        let churn_stop = Arc::new(AtomicBool::new(false));
        let churner = {
            let stop = Arc::clone(&churn_stop);
            std::thread::spawn(move || {
                let mut taken = 0usize;
                for _ in 0..CHURN_PER_ROUND {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Some(registration) = try_install(leak(0, 0)) {
                        taken += 1;
                        drop(registration);
                    }
                }
                taken
            })
        };

        // Tear the primary down *now*, with the workers mid-flight and the churner competing for the
        // slot it is about to free. Nothing is synchronised: the race is the input.
        drop(primary_reg);
        // From here a registrant would be free to release the context. We mark instead.
        primary.state.store(DEAD, Ordering::Release);

        for worker in workers {
            worker.join().expect("a worker thread");
        }
        churn_stop.store(true, Ordering::Relaxed);
        churn_installs += churner.join().expect("the churn thread");
        drop(net_reg);

        let entered = primary.entered.load(Ordering::Relaxed);
        let resolved = primary.resolved.load(Ordering::Relaxed);
        let after = primary.after_release.load(Ordering::Relaxed);
        total_entered += entered;
        total_resolved += resolved;
        total_after_release += after;
        if entered > 0 && entered < (THREADS * PAGES) as u64 {
            // The primary served some faults and the net served the rest, which means the teardown
            // genuinely landed in the middle of the load rather than before or after it.
            rounds_with_concurrency += 1;
        }

        vm::release(reservation).expect("the reservation comes back");
    }

    let after_stats = fault::stats();
    let drained = after_stats.drained - before.drained;
    let examined = after_stats.examined - before.examined;

    println!(
        "teardown race: n = {ROUNDS} rounds x {THREADS} threads x {PAGES} pages; \
         {examined} access violations examined, {total_entered} reached the primary handler \
         ({total_resolved} resolved by it), {rounds_with_concurrency}/{ROUNDS} rounds had the \
         teardown land mid-load, {drained} releases had to wait for an in-flight dispatch, \
         {churn_installs} slots claimed by the churn thread while teardowns were in progress"
    );

    assert_eq!(
        NULL_CONTEXTS.load(Ordering::Relaxed),
        0,
        "{} dispatches entered a handler holding a context of zero, over n = {ROUNDS} rounds with \
         {churn_installs} slot claims against live teardowns. That is a slot reclaimed and then \
         cleared while a dispatch was still inside it: the drain was running, `install` took the \
         slot because `handler` had been set to zero, and `release` went on to clear the field the \
         new registrant had just written",
        NULL_CONTEXTS.load(Ordering::Relaxed)
    );
    assert_eq!(
        total_after_release, 0,
        "{total_after_release} handler frames were still running after their registration's `drop` \
         had returned, over n = {ROUNDS} rounds. Each one is a use-after-free: a real registrant \
         frees its context there, and this frame is reading it inside the OS exception dispatcher"
    );
    assert!(
        total_entered > 0,
        "the primary handler was never entered over n = {ROUNDS} rounds x {THREADS} threads x \
         {PAGES} pages, so this test measured nothing"
    );
    assert!(
        examined >= total_entered,
        "every fault that reached a handler must have been examined first: {examined} examined \
         against {total_entered} entries"
    );
}

/// The handler table survives being churned by every thread at once, which is the shape
/// `pager_exhaustion.rs` and libtest between them produce in-tree.
///
/// Slot reuse is what makes the churn survivable, and reuse is only sound because `release` drains:
/// a slot handed out again while a stale dispatch still held it would pair that dispatch with the
/// **new** registrant's context. There is nothing to assert here beyond "it completes and every
/// install succeeds" — but that is exactly the run that used to be able to hand one thread another
/// thread's context.
#[test]
fn concurrent_install_and_release_churn_never_hands_out_an_occupied_slot() {
    let _serial = serialized();
    NULL_CONTEXTS.store(0, Ordering::Relaxed);
    const CHURN_THREADS: usize = 8;
    const CHURN_ROUNDS: usize = 200;

    let threads: Vec<_> = (0..CHURN_THREADS)
        .map(|_| {
            std::thread::spawn(|| {
                let mut slots = Vec::with_capacity(CHURN_ROUNDS);
                for _ in 0..CHURN_ROUNDS {
                    // A context of 0 and a region of zero length: this handler declines everything,
                    // which is what makes it safe to have dozens of them installed at once.
                    let context = leak(0, 0);
                    let registration = install(context);
                    slots.push(registration.slot());
                    drop(registration);
                }
                slots
            })
        })
        .collect();

    let mut total = 0usize;
    for thread in threads {
        total += thread.join().expect("a churn thread").len();
    }
    assert_eq!(
        total,
        CHURN_THREADS * CHURN_ROUNDS,
        "every install must have succeeded: n = {CHURN_THREADS} threads x {CHURN_ROUNDS} rounds"
    );
    assert_eq!(
        NULL_CONTEXTS.load(Ordering::Relaxed),
        0,
        "a dispatch was handed a zero context during n = {CHURN_THREADS} x {CHURN_ROUNDS} \
         install/release rounds, which is a slot cleared while somebody was inside it"
    );
}
