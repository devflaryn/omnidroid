//! Demand paging for guest memory, driven by the host's own access violations.
//!
//! # What this is for, and what it is deliberately *not* for
//!
//! D10 rejected fault-driven paging as the runtime's **commit driver**, on measurement: a vectored
//! fault costs **2053 ns** against **3 ns/page** for bulk commit, and the fault path saturates at
//! about 1.3 M faults/s process-wide. Nothing here contradicts that. The loader still commits
//! explicitly with [`GuestSpace::ensure_committed`], and it still should.
//!
//! This exists for two other reasons, both of which arrived with the CPU backend:
//!
//! 1. **Ownership.** D4's identity mapping means a guest load *is* a host load, so a guest access to
//!    a page the guest has mapped but Omnidroid has not committed is an ordinary access violation
//!    inside JIT-generated code. Somebody handles it. If we do not, dynarmic's frame-based SEH does,
//!    and that permanently deoptimizes the block onto the callback path
//!    (`recompile_on_fastmem_failure`) — measured **30-49x** slower through the CPU backend's own
//!    callbacks (n = 31, two loop shapes), taken silently, with correct results. D10 requires
//!    Omnidroid to keep guest paging; this is the mechanism that keeps it.
//! 2. **A typed stop instead of a crash.** Guest code is untrusted by construction (Global
//!    Constraint 11) and will dereference garbage. An address this pager declines continues to
//!    normal dispatch, reaches the CPU backend's slow-path callback, and becomes a typed exit. That
//!    only works if the declining is deliberate and fast.
//!
//! # The two invariants a caller must hold
//!
//! * **The thread running guest code must not hold this space's internal lock.** The handler calls
//!   [`GuestSpace::region_at`] and [`GuestSpace::ensure_committed`], both of which take it, and an
//!   access violation can arrive at any instruction. Nothing in `GuestCpu`'s shape lets a caller
//!   hold it across `run`, so this holds structurally today; it is written down because a future
//!   callback that touched `GuestSpace` while the guest was running would deadlock, not fail.
//! * **The space must outlive the pager.** Enforced by ownership: [`DemandPager`] holds an
//!   [`Arc<GuestSpace>`], so the space cannot be dropped — or `close`d, which consumes it — while a
//!   handler that reads it is still published. An `&'space GuestSpace` would have been tighter, but
//!   it cannot be held by the CPU backend, whose `create_thread` returns a `Box<dyn GuestCpu>` with
//!   no lifetime to borrow from.
//!
//! A re-entrancy guard catches the first invariant being broken *on one thread* — a fault raised
//! inside the handler declines immediately rather than recursing — but it cannot catch the deadlock
//! case, which is why the invariant is stated rather than merely guarded.

use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omni_platform::fault::{
    self, Fault, FaultError, FaultOutcome, FaultRegistration,
};

use crate::space::{GuestAddr, GuestSpace};

/// What a [`DemandPager`] has done since it was installed.
///
/// # The invariant
///
/// **`examined == resolved + declined`**, always. It did not hold before: `examined` was incremented
/// inside `resolve`, while two paths that return *before* `resolve` — a nested fault and a contained
/// panic — incremented `declined`, so `declined > examined` was representable and nothing said it
/// should not be. A counter set with no stated relationship to its siblings is a number nobody can
/// check, which is the opposite of what these exist for.
///
/// [`DemandPager::stats_are_consistent`] is the assertion, and `reentered` is what let the invariant
/// be made true rather than merely documented: the nested-fault path is now counted like any other
/// examination, and the thing it used to be the only witness of has a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PagerStats {
    /// Faults whose address fell inside this space, so the pager looked at them.
    ///
    /// Faults outside the space are declined before anything is counted: they belong to some other
    /// part of the process and counting them would make this number a property of the whole
    /// process rather than of the guest.
    pub examined: u64,
    /// Faults the pager resolved, either by committing memory or by finding the page already
    /// accessible.
    pub resolved: u64,
    /// Bytes of commit charge the pager has taken. This is the number D10 cares about.
    pub bytes_committed: u64,
    /// Faults inside the space that the pager declined — an unmapped address, a protection the
    /// access is not allowed by, a commit that failed, a nested fault, or a contained panic. Each of
    /// these goes on to become a typed CPU exit rather than being resolved here.
    pub declined: u64,
    /// Declines that happened because the fault arrived while this thread was already inside the
    /// handler.
    ///
    /// A subset of `declined`, not an addition to it. Non-zero means the pager faulted on itself,
    /// which is a defect in Omnidroid rather than anything guest code can provoke — so it is the one
    /// counter here whose expected value is exactly zero.
    pub reentered: u64,
    /// Zero-commit resolutions declined because this thread had already retried the same commit
    /// granule.
    ///
    /// The bound in `resolve_without_committing`, made visible. A non-zero value means a fault the
    /// pager believed it had fixed came back, which is a disagreement between the region map and the
    /// OS; the decline is what turns that into a typed guest fault instead of a spin.
    pub retries_exhausted: u64,
}

impl PagerStats {
    /// Whether the invariant in this type's docs holds: every examined fault was either resolved or
    /// declined, exactly once, and the two named subsets of `declined` fit inside it.
    ///
    /// A method rather than an internal assertion, because these counters are `Relaxed` and read
    /// without a lock: a reader that catches a handler mid-flight can legitimately see `examined`
    /// incremented before its outcome. So this is for a caller that knows nothing is running — every
    /// test that uses it joins its guest threads first — and for that caller it is exact.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.examined == self.resolved + self.declined
            && self.reentered <= self.declined
            && self.retries_exhausted <= self.declined
    }
}

thread_local! {
    /// Set while this thread is inside the handler. A fault raised *by* the handler declines
    /// immediately instead of recursing forever.
    static IN_HANDLER: Cell<bool> = const { Cell::new(false) };

    /// The last commit **granule** this thread resolved without committing anything, or 0.
    ///
    /// Two changes from the address it used to hold, both from the whole-branch review.
    ///
    /// **A granule, not an address.** The documented bound was "one retry per thread per address",
    /// and a fault alternating between two addresses *in the same granule* defeated it exactly: each
    /// arrival saw a different address, so neither was ever a repeat, and the pair looped. The unit
    /// the decision is about was always the granule — `ensure_committed` commits granules — so
    /// that is what is remembered, and the two-address alternation collapses to one entry.
    ///
    /// **Per thread, still.** The whole point of the zero-commit path is that two *different*
    /// threads legitimately see the same granule at the same instant.
    static LAST_ZERO_COMMIT_GRANULE: Cell<usize> = const { Cell::new(0) };

    /// Consecutive zero-commit resolutions on this thread, with no real commit in between.
    ///
    /// The granule record above is precise and is not a *bound*: a fault cycling across three or
    /// more distinct granules repeats none of them consecutively. This is the bound. It is generous
    /// on purpose, because the legitimate case is unbounded in principle — a thread can race ahead
    /// of another that is committing granules and see a long run of "somebody else got there first"
    /// — and declining one of those costs the 30-49x deoptimization this whole module exists to
    /// avoid. A run of [`MAX_ZERO_COMMIT_STREAK`] is two milliseconds of solid faulting at D10's
    /// measured 2053 ns per fault, and the decline that ends it ends the loop: a declined fault
    /// leaves the pager and becomes a typed exit or a deoptimized-but-correct access.
    static ZERO_COMMIT_STREAK: Cell<u32> = const { Cell::new(0) };
}

/// Consecutive zero-commit resolutions one thread may take before the next is declined.
///
/// A fitted constant, not a derived one (Global Constraint 12): it is chosen to be far above any
/// legitimate run and far below a hang. At D10's measured 2053 ns per vectored fault, 1024 of them is
/// about 2.1 ms.
const MAX_ZERO_COMMIT_STREAK: u32 = 1024;

/// Per-pager state the fault handler reaches through an opaque `usize`.
///
/// Boxed and never moved, because its address is published to the process-wide handler table.
struct PagerInner {
    space: Arc<GuestSpace>,
    base: GuestAddr,
    end: GuestAddr,
    /// The space's commit granule, copied here so the handler does not take the space's lock to ask
    /// for it. It is fixed at construction, so a copy cannot go stale.
    granule: usize,
    examined: AtomicU64,
    resolved: AtomicU64,
    bytes_committed: AtomicU64,
    declined: AtomicU64,
    reentered: AtomicU64,
    retries_exhausted: AtomicU64,
}

/// Faults every pager in the process has resolved, and bytes they committed doing it.
///
/// Process-wide rather than per pager so a diagnostic holding no pager -- `omni-android`'s interval
/// reporter -- can read them. Two relaxed increments on a path that has just taken a hardware fault
/// and a system call.
static PROCESS_RESOLVED: AtomicU64 = AtomicU64::new(0);
static PROCESS_BYTES_COMMITTED: AtomicU64 = AtomicU64::new(0);

/// `(faults resolved, bytes committed)` by every demand pager this process has installed, since the
/// process started.
#[must_use]
pub fn process_pager_totals() -> (u64, u64) {
    (PROCESS_RESOLVED.load(Ordering::Relaxed), PROCESS_BYTES_COMMITTED.load(Ordering::Relaxed))
}

impl PagerInner {
    /// Count one examined fault and its outcome, in one place.
    ///
    /// Every in-space fault goes through here exactly once, which is what makes
    /// `examined == resolved + declined` true by construction rather than by inspection.
    fn record(&self, outcome: FaultOutcome) -> FaultOutcome {
        self.examined.fetch_add(1, Ordering::Relaxed);
        match outcome {
            FaultOutcome::Resolved => {
                PROCESS_RESOLVED.fetch_add(1, Ordering::Relaxed);
                self.resolved.fetch_add(1, Ordering::Relaxed)
            }
            FaultOutcome::NotOurs => self.declined.fetch_add(1, Ordering::Relaxed),
        };
        outcome
    }
}

/// Serves guest access violations for one [`GuestSpace`] for as long as it is alive.
///
/// Drop order is load-bearing: the registration field is declared first, so it is released — and
/// the handler therefore stops being callable — before the state it reads is freed.
///
/// **That field order was necessary and was never sufficient**, and the whole-branch review is what
/// showed it. Releasing a slot stops calls that have not started; it cannot stop one already in
/// flight, because the vectored dispatcher is process-wide and can be preempted between loading the
/// handler and calling it. Nothing this type can do about its own fields closes that window. It is
/// closed on the other side instead: dropping a `FaultRegistration` now blocks until every dispatch
/// inside the handler has returned, so by the time `inner` is freed here, no frame can be holding
/// its address. The ordering below still matters — it is what makes the drain *start* before the
/// free — but the guarantee it rests on is `omni-platform`'s, and is documented there.
pub struct DemandPager {
    registration: FaultRegistration,
    inner: Box<PagerInner>,
}

impl DemandPager {
    /// Install a pager for `space`.
    ///
    /// # Errors
    ///
    /// [`FaultError::Unsupported`] on a target with no vectored-handler implementation (Linux,
    /// macOS), [`FaultError::HandlerTableFull`], or [`FaultError::Os`].
    pub fn install(space: Arc<GuestSpace>) -> Result<Self, FaultError> {
        let inner = Box::new(PagerInner {
            base: space.base(),
            end: space.end(),
            granule: space.commit_granule(),
            space,
            examined: AtomicU64::new(0),
            resolved: AtomicU64::new(0),
            bytes_committed: AtomicU64::new(0),
            declined: AtomicU64::new(0),
            reentered: AtomicU64::new(0),
            retries_exhausted: AtomicU64::new(0),
        });
        // The address is stable for as long as the box is: `inner` is never moved out of, and the
        // registration that publishes this address is dropped before the box is.
        let context = (&*inner) as *const PagerInner as usize;
        // SAFETY: `fault::install`'s four conditions, in order.
        //
        // * `context` is the address of a `Box` this `DemandPager` owns and never moves out of, and
        //   `registration` is declared before `inner`, so it is dropped first -- and that drop is a
        //   quiescence point: it clears the slot and then waits for every dispatch already inside
        //   `handle_fault` to return. So the box is freed only once no frame can hold its address.
        //   Field order alone would not be enough, and used not to be; see the type docs.
        // * `handle_fault` wraps its whole body in `catch_unwind` and reports a panic as
        //   `NotOurs`, so nothing unwinds into the dispatcher, and it always returns: its longest
        //   path is one region lookup plus one `ensure_committed`, both bounded.
        // * It takes this space's internal lock, and the module docs state the matching invariant:
        //   the thread running guest code never holds it. A thread-local guard breaks the
        //   single-thread version of a violation.
        // * It returns `Resolved` only when the page really is accessible afterwards: either this
        //   call committed it, or another thread committed the same granule a moment earlier. The
        //   second case is bounded twice -- one retry per thread per commit **granule**, and a
        //   ceiling on consecutive zero-commit resolutions -- so a disagreement between the region
        //   map and the OS becomes a typed guest fault rather than a fault loop. It used to be
        //   bounded per *address*, which two addresses in one granule defeated.
        let registration = unsafe { fault::install(handle_fault, context)? };
        Ok(Self { registration, inner })
    }

    /// What this pager has done. See [`PagerStats`].
    #[must_use]
    pub fn stats(&self) -> PagerStats {
        PagerStats {
            examined: self.inner.examined.load(Ordering::Relaxed),
            resolved: self.inner.resolved.load(Ordering::Relaxed),
            bytes_committed: self.inner.bytes_committed.load(Ordering::Relaxed),
            declined: self.inner.declined.load(Ordering::Relaxed),
            reentered: self.inner.reentered.load(Ordering::Relaxed),
            retries_exhausted: self.inner.retries_exhausted.load(Ordering::Relaxed),
        }
    }

    /// Whether [`PagerStats::is_consistent`] holds for this pager right now.
    #[must_use]
    pub fn stats_are_consistent(&self) -> bool {
        self.stats().is_consistent()
    }

    /// Which handler slot this pager holds, for diagnostics.
    #[must_use]
    pub fn slot(&self) -> usize {
        self.registration.slot()
    }
}

impl core::fmt::Debug for DemandPager {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DemandPager")
            .field("base", &format_args!("{:#x}", self.inner.base))
            .field("end", &format_args!("{:#x}", self.inner.end))
            .field("slot", &self.registration.slot())
            .field("stats", &self.stats())
            .finish()
    }
}

/// The handler published to `omni-platform`'s vectored dispatch.
///
/// Runs on the faulting thread with that thread frozen mid-instruction. It must not unwind, so the
/// body is wrapped in [`catch_unwind`] and a panic is reported as "not ours" — declining is always
/// safe, where unwinding into the OS exception dispatcher is undefined behaviour.
fn handle_fault(context: usize, fault: &Fault) -> FaultOutcome {
    let inner = context as *const PagerInner;
    if inner.is_null() {
        return FaultOutcome::NotOurs;
    }
    // SAFETY: `context` is the address of the `Box<PagerInner>` owned by the `DemandPager` whose
    // registration published it. The registration is dropped before the box (field order in
    // `DemandPager`), and that drop clears the slot and then **waits for this function to return**
    // on every thread already inside it. So neither a call that has begun nor one about to begin can
    // outlive the box: the first is drained, the second never starts.
    let inner: &PagerInner = unsafe { &*inner };

    // A cheap bounds check before anything else: every access violation in the process arrives
    // here, and almost none of them are ours.
    if fault.address < inner.base || fault.address >= inner.end {
        return FaultOutcome::NotOurs;
    }

    let reentered = IN_HANDLER.with(|flag| flag.replace(true));
    if reentered {
        // A fault inside the handler. Declining breaks the recursion; resolving could not, because
        // whatever the inner fault was, this frame has not finished the work that would fix it.
        //
        // Counted as an examination *and* a decline, which is what keeps `PagerStats`'s invariant
        // true. `reentered` is the separate witness that this path was the one taken — it used to be
        // inferable only from `examined` staying at zero, which is what made the invariant false.
        inner.reentered.fetch_add(1, Ordering::Relaxed);
        return inner.record(FaultOutcome::NotOurs);
    }

    // A panic in the handler declines, because unwinding into the OS exception dispatcher is
    // undefined behaviour and declining always is safe. But it must be **counted**: a
    // resolvable fault declined here goes to dynarmic's frame-based handler, which
    // recompiles the block with fastmem off and puts it on the 30-49x path for good. That is
    // the same silent deoptimization the concurrent-granule bug caused, and worse, because
    // an uncounted decline is invisible even in `PagerStats` -- there would be nothing to
    // look at.
    let outcome = match catch_unwind(AssertUnwindSafe(|| resolve(inner, fault))) {
        Ok(outcome) => outcome,
        Err(_) => FaultOutcome::NotOurs,
    };
    let outcome = inner.record(outcome);

    IN_HANDLER.with(|flag| flag.set(false));
    outcome
}

/// Ask the shared policy, and turn its answer into a dispatch outcome.
///
/// The policy itself is [`crate::access::admit`] and is the *same* function `omni-cpu`'s slow-path
/// callback calls — see that module for why there used to be two of them with different rules. What
/// is left here is the part that is genuinely the pager's: one byte, because that is what an access
/// violation reports, and the zero-commit decision below, which only a fault handler has to make.
fn resolve(inner: &PagerInner, fault: &Fault) -> FaultOutcome {
    // One byte is the right request, and it is the whole of the divergence from the CPU side: the
    // hardware names the address that could not be reached, and an access straddling a page boundary
    // faults again on the second page. `ensure_committed` inside `admit` expands outwards to whole
    // commit granules and clips to the mapping, so this commits exactly one granule of the mapping
    // that was touched — D10's measured 150 ns/page at the 64 KiB granule.
    match crate::access::admit(&inner.space, fault.address, 1, fault.access) {
        Ok(admitted) if admitted.committed > 0 => {
            inner
                .bytes_committed
                .fetch_add(admitted.committed as u64, Ordering::Relaxed);
            PROCESS_BYTES_COMMITTED.fetch_add(admitted.committed as u64, Ordering::Relaxed);
            // A real commit clears the retry record: progress was made, so whatever the thread sees
            // next is a new question rather than the same one again.
            LAST_ZERO_COMMIT_GRANULE.with(|cell| cell.set(0));
            ZERO_COMMIT_STREAK.with(|cell| cell.set(0));
            FaultOutcome::Resolved
        }
        Ok(admitted) => resolve_without_committing(inner, fault, admitted.anonymous),
        // Unmapped, or a protection the access is not allowed by. A write to a read-only guest page
        // is not something to commit our way out of: it becomes a typed memory fault naming the
        // address, which is what a real kernel would deliver to the guest.
        //
        // Or the commit failed — `ERROR_COMMITMENT_LIMIT`, or this space's own ceiling (D15).
        // Declining turns that into a typed guest memory fault instead of an infinite fault loop.
        // The refusal is not logged: this is an exception handler, and formatting allocates.
        Err(_) => FaultOutcome::NotOurs,
    }
}

/// `ensure_committed` committed nothing. Decide whether that means the fault is already fixed or
/// cannot be fixed here.
///
/// **This distinction was wrong, and a concurrency test found it.** The original code declined every
/// zero, on the reasoning that "already committed" means the fault was not a missing commit. Under
/// several guest threads that is false and common: two threads fault on pages of the *same* 64 KiB
/// commit granule at the same moment, the first commits it, and the second's `ensure_committed`
/// correctly returns 0 — the page *is* now accessible, and retrying the instruction would succeed.
/// Declining instead handed the fault to dynarmic's frame-based handler, which recompiled the block
/// with fastmem off and put it permanently on the callback path (measured 30-49x). Correct results,
/// silently slower, triggered only by timing: exactly the failure mode D4's assertion exists for,
/// arriving by a route the assertion cannot see.
///
/// So a zero on an **anonymous** mapping whose protection already permits the access is treated as
/// resolved. The only thing that can then still fault is a disagreement between our region map and
/// the OS, which would be a defect here rather than a guest input — and the retry counter below
/// bounds it to one extra fault rather than a loop.
fn resolve_without_committing(
    inner: &PagerInner,
    fault: &Fault,
    anonymous: bool,
) -> FaultOutcome {
    // Both bounds, and they answer different questions. The granule record catches the shape the
    // review found — a fault alternating between two addresses of one granule, which the old
    // per-address record could never see as a repeat — and the streak is the bound proper, for a
    // cycle across three or more granules that the record alone would never call repeated.
    let granule = fault.address - fault.address % inner.granule.max(1);
    let repeated = LAST_ZERO_COMMIT_GRANULE.with(|cell| cell.replace(granule)) == granule;
    let streak = ZERO_COMMIT_STREAK.with(|cell| {
        let next = cell.get().saturating_add(1);
        cell.set(next);
        next
    });
    let exhausted = repeated || streak > MAX_ZERO_COMMIT_STREAK;

    if anonymous && !exhausted {
        return FaultOutcome::Resolved;
    }
    if anonymous {
        // The bound fired. Counted separately, because "the pager believed it had fixed this and it
        // came back" is a disagreement between the region map and the OS — a defect here rather than
        // a guest input — and it must not be invisible.
        inner.retries_exhausted.fetch_add(1, Ordering::Relaxed);
        ZERO_COMMIT_STREAK.with(|cell| cell.set(0));
    }
    // Either file-backed — where no commit is owed and a fault means something else entirely — or a
    // retry that did not help.
    FaultOutcome::NotOurs
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::Protection;
    use omni_platform::fault::FaultAccess;

    /// A `PagerInner` with no registration behind it, for driving the decision functions directly.
    ///
    /// Deliberate: every test below is about a decision, and a decision is testable without a real
    /// access violation. The concurrency test in `omni-cpu` that first found the zero-commit bug
    /// cannot be what *pins* it — a mutation row backed by it passed and failed on alternate runs,
    /// and Task 1's lesson is that a flaky row attributes a mutation to the wrong detector.
    fn inner_over(space: Arc<GuestSpace>) -> PagerInner {
        PagerInner {
            base: space.base(),
            end: space.end(),
            granule: space.commit_granule(),
            space,
            examined: AtomicU64::new(0),
            resolved: AtomicU64::new(0),
            bytes_committed: AtomicU64::new(0),
            declined: AtomicU64::new(0),
            reentered: AtomicU64::new(0),
            retries_exhausted: AtomicU64::new(0),
        }
    }

    fn stats_of(inner: &PagerInner) -> PagerStats {
        PagerStats {
            examined: inner.examined.load(Ordering::Relaxed),
            resolved: inner.resolved.load(Ordering::Relaxed),
            bytes_committed: inner.bytes_committed.load(Ordering::Relaxed),
            declined: inner.declined.load(Ordering::Relaxed),
            reentered: inner.reentered.load(Ordering::Relaxed),
            retries_exhausted: inner.retries_exhausted.load(Ordering::Relaxed),
        }
    }

    fn reset_thread_state() {
        IN_HANDLER.with(|flag| flag.set(false));
        LAST_ZERO_COMMIT_GRANULE.with(|cell| cell.set(0));
        ZERO_COMMIT_STREAK.with(|cell| cell.set(0));
    }

    /// A fault raised **inside the handler**, on the same thread.
    ///
    /// The guard is what stops that recursing without end, and it is the one piece of this module
    /// that cannot be provoked from guest code: a genuinely nested access violation would have to be
    /// a defect in the pager itself. So it is driven directly — `handle_fault` is a plain `fn`, and
    /// the thread-local is this module's — which makes the test deterministic rather than a race.
    ///
    /// What the guard does and does not buy, stated because the difference matters: it bounds the
    /// recursion, so the inner fault is declined immediately and goes on to normal dispatch. It does
    /// **not** make an unresolvable nested fault survivable, and it should not — that is a bug in
    /// Omnidroid, not untrusted guest input, and the honest outcome for it is a crash rather than a
    /// silent loop.
    #[test]
    fn a_fault_raised_inside_the_handler_declines_instead_of_recursing() {
        reset_thread_state();
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let base = space.base();
        let inner = inner_over(space);
        let context = (&inner) as *const PagerInner as usize;
        let fault = Fault {
            address: base + 0x1000,
            access: FaultAccess::Read,
            instruction_pointer: 0,
        };

        // Exactly what the outer frame has done by the time a nested fault arrives.
        let previous = IN_HANDLER.with(|flag| flag.replace(true));
        let outcome = handle_fault(context, &fault);
        IN_HANDLER.with(|flag| flag.set(previous));

        assert_eq!(
            outcome,
            FaultOutcome::NotOurs,
            "a fault raised inside the handler must decline, or the handler re-enters itself \
             for every level of a recursion that has no bottom"
        );
        let nested = stats_of(&inner);
        assert_eq!(
            nested.declined, 1,
            "and it must be counted, so a pager that is faulting on itself is visible"
        );
        assert_eq!(
            nested.reentered, 1,
            "the nested path must be identifiable on its own. This is the witness that used to be \
             `examined == 0`, which is the same statement -- the nested call returns before \
             `resolve`, which is where the space lock is taken -- but which made \
             `declined > examined` representable and the stats invariant false"
        );
        assert_eq!(nested.resolved, 0);
        assert_eq!(
            nested.examined,
            nested.resolved + nested.declined,
            "examined == resolved + declined, on the path that used to break it"
        );

        // And the flag is left clear afterwards, so one nested fault does not wedge the thread out
        // of ever serving another.
        assert!(!IN_HANDLER.with(Cell::get));
        assert_eq!(handle_fault(context, &fault), FaultOutcome::NotOurs, "no mapping there");
        let after = stats_of(&inner);
        assert_eq!(after.examined, 2, "this one did reach `resolve`");
        assert_eq!(after.reentered, 1, "and it was not a re-entry");
        assert_eq!(after.examined, after.resolved + after.declined);
    }

    /// The zero-commit decision, driven directly so that it is **deterministic**.
    ///
    /// `ensure_committed` returning 0 means "nothing was owed". Under several guest threads that is
    /// common and does *not* mean the fault is unfixable: two threads fault on pages of the same
    /// 64 KiB commit granule at the same moment, the first commits it, and the second's request
    /// correctly commits nothing — the page *is* now accessible, and retrying the instruction
    /// succeeds. Declining instead handed the fault to dynarmic's frame-based handler, which
    /// recompiled the block with fastmem off and put it permanently on the callback path (measured
    /// 30-49x). Correct results, silently slower, triggered only by timing.
    #[test]
    fn a_commit_that_committed_nothing_is_resolved_once_per_granule() {
        reset_thread_state();
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let base = space.base();
        let granule = space.commit_granule();
        let inner = inner_over(space);
        let fault_at = |address| Fault {
            address,
            access: FaultAccess::Write,
            instruction_pointer: 0,
        };

        // Anonymous, first time in this granule: another thread committed it a moment ago, so the
        // page IS accessible and retrying the instruction succeeds.
        assert_eq!(
            resolve_without_committing(&inner, &fault_at(base + 0x2000), true),
            FaultOutcome::Resolved
        );

        // **The defect the review found.** A second fault at a *different address in the same
        // granule* is the same question asked again, so it must be refused. The record used to hold
        // an address, so this arrived as a first-time address and was resolved again — and a fault
        // alternating between two such addresses looped, with the bound documented as holding.
        assert_eq!(
            resolve_without_committing(&inner, &fault_at(base + 0x3000), true),
            FaultOutcome::NotOurs,
            "two addresses in one commit granule are one retry, not two: the unit the decision is \
             about is the granule, because that is what `ensure_committed` commits"
        );
        assert_eq!(stats_of(&inner).retries_exhausted, 1, "and the bound firing is visible");

        // A *different* granule is a new question, and is resolved once.
        assert_eq!(
            resolve_without_committing(&inner, &fault_at(base + granule + 0x2000), true),
            FaultOutcome::Resolved,
            "a genuinely different granule must not be refused by another granule's retry"
        );

        // File-backed: no commit is owed, so a fault there means something this pager cannot fix.
        // It is not a retry, so it must not be counted as one.
        reset_thread_state();
        let before = stats_of(&inner).retries_exhausted;
        assert_eq!(
            resolve_without_committing(&inner, &fault_at(base + 0x2000), false),
            FaultOutcome::NotOurs
        );
        assert_eq!(
            stats_of(&inner).retries_exhausted,
            before,
            "a file-backed decline is not an exhausted retry"
        );
    }

    /// The **bound**, as opposed to the granule record: a cycle across more granules than the record
    /// can remember must still terminate.
    ///
    /// The record holds one granule, so a fault walking three or more of them in a ring repeats none
    /// of them consecutively and would be resolved every time — the loop the review's "one retry per
    /// thread per address" was supposed to have ruled out, arriving by a slightly longer route. The
    /// streak counter is what actually bounds it, and this is the test that says the bound exists at
    /// all rather than that it is tight.
    #[test]
    fn a_long_cycle_of_distinct_granules_is_still_bounded() {
        reset_thread_state();
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let base = space.base();
        let granule = space.commit_granule();
        let inner = inner_over(space);

        // Four distinct granules, walked in a ring so that no two consecutive faults share one.
        const RING: usize = 4;
        let mut resolved = 0u32;
        let mut declined = 0u32;
        for i in 0..(MAX_ZERO_COMMIT_STREAK as usize + RING * 2) {
            let fault = Fault {
                address: base + (i % RING) * granule + 0x1000,
                access: FaultAccess::Write,
                instruction_pointer: 0,
            };
            match resolve_without_committing(&inner, &fault, true) {
                FaultOutcome::Resolved => resolved += 1,
                FaultOutcome::NotOurs => declined += 1,
            }
        }
        assert!(
            declined > 0,
            "n = {} faults across {RING} distinct granules, none consecutive: every one was \
             resolved, so nothing bounds the loop and a guest thread in this state never makes \
             progress",
            MAX_ZERO_COMMIT_STREAK as usize + RING * 2
        );
        assert_eq!(
            stats_of(&inner).retries_exhausted,
            u64::from(declined),
            "every decline on this path is an exhausted retry and must be counted as one"
        );
        // **An absolute floor, deliberately not `MAX_ZERO_COMMIT_STREAK`.** A bound stated relative
        // to the constant it is bounding cannot notice the constant moving, which is the row
        // `mem-B8` exists to prove: tightening the streak to 1 passes a relative check and destroys
        // the property. So the floor is a number of its own — a thread racing another that is
        // committing granules ahead of it must be allowed a long run of these, because declining one
        // costs the 30-49x deoptimization this whole module exists to avoid, and the observed race
        // involves a handful of granules against this two-orders-of-magnitude headroom.
        const LEGITIMATE_RUN: u32 = 256;
        assert!(
            resolved >= LEGITIMATE_RUN,
            "the bound must be loose enough to leave the legitimate case alone: only {resolved} of \
             the run were resolved, against a floor of {LEGITIMATE_RUN} consecutive zero-commit \
             resolutions one thread must be allowed"
        );
    }

    /// A real commit resets both bounds, because progress was made.
    ///
    /// Without this, a thread that legitimately alternates between committing a granule and racing
    /// another thread for the next one would accumulate a streak and eventually be refused.
    #[test]
    fn a_real_commit_clears_the_retry_record() {
        reset_thread_state();
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let base = space.base();
        let inner = inner_over(space);
        let fault = Fault {
            address: base + 0x2000,
            access: FaultAccess::Write,
            instruction_pointer: 0,
        };

        assert_eq!(resolve_without_committing(&inner, &fault, true), FaultOutcome::Resolved);
        // Exactly what `resolve` does on a commit of non-zero size.
        LAST_ZERO_COMMIT_GRANULE.with(|cell| cell.set(0));
        ZERO_COMMIT_STREAK.with(|cell| cell.set(0));
        assert_eq!(
            resolve_without_committing(&inner, &fault, true),
            FaultOutcome::Resolved,
            "the same granule after a real commit is a new question, not a repeat"
        );
        assert_eq!(stats_of(&inner).retries_exhausted, 0);
    }

    /// The permission table lives in [`crate::access`] now, with both crates' rules, and is pinned
    /// there over every protection and every access. What is checked here is that this module still
    /// asks *that* predicate — a local copy reappearing is the defect the review named.
    #[test]
    fn the_pager_uses_the_shared_access_policy() {
        use FaultAccess::{Execute, Read, Write};
        for protection in Protection::ALL {
            for access in [Read, Write, Execute] {
                assert_eq!(
                    crate::access::permits(protection, access),
                    match access {
                        Read => protection.is_readable(),
                        Write => protection.is_writable(),
                        Execute => protection.is_executable(),
                    },
                    "{protection} {access}"
                );
            }
        }
    }
}
