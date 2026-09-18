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
    self, Fault, FaultAccess, FaultError, FaultOutcome, FaultRegistration,
};

use crate::space::{GuestAddr, GuestSpace};
use crate::{Protection, RegionKind};

/// What a [`DemandPager`] has done since it was installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PagerStats {
    /// Faults whose address fell inside this space, so the pager actually looked at them.
    ///
    /// Faults outside the space are declined before anything is counted: they belong to some other
    /// part of the process and counting them would make this number a property of the whole
    /// process rather than of the guest.
    pub examined: u64,
    /// Faults the pager resolved by committing memory.
    pub resolved: u64,
    /// Bytes of commit charge the pager has taken. This is the number D10 cares about.
    pub bytes_committed: u64,
    /// Faults inside the space that the pager declined — an unmapped address, a protection the
    /// access is not allowed by, or a commit that failed. Each of these goes on to become a typed
    /// CPU exit rather than being resolved here.
    pub declined: u64,
}

thread_local! {
    /// Set while this thread is inside the handler. A fault raised *by* the handler declines
    /// immediately instead of recursing forever.
    static IN_HANDLER: Cell<bool> = const { Cell::new(false) };

    /// The last address this thread resolved *without* committing anything, or 0.
    ///
    /// The bound on the one case that could otherwise loop: see `resolve_without_committing`. Two
    /// faults running at the same address on one thread means retrying did not help, so the second
    /// declines and the fault becomes a typed guest exit. Per thread rather than shared, because
    /// the whole point of the path is that two *different* threads legitimately see the same
    /// granule.
    static LAST_ZERO_COMMIT: Cell<usize> = const { Cell::new(0) };
}

/// Per-pager state the fault handler reaches through an opaque `usize`.
///
/// Boxed and never moved, because its address is published to the process-wide handler table.
struct PagerInner {
    space: Arc<GuestSpace>,
    base: GuestAddr,
    end: GuestAddr,
    examined: AtomicU64,
    resolved: AtomicU64,
    bytes_committed: AtomicU64,
    declined: AtomicU64,
}

/// Serves guest access violations for one [`GuestSpace`] for as long as it is alive.
///
/// Drop order is load-bearing: the registration field is declared first, so it is released — and
/// the handler therefore stops being callable — before the state it reads is freed.
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
            space,
            examined: AtomicU64::new(0),
            resolved: AtomicU64::new(0),
            bytes_committed: AtomicU64::new(0),
            declined: AtomicU64::new(0),
        });
        // The address is stable for as long as the box is: `inner` is never moved out of, and the
        // registration that publishes this address is dropped before the box is.
        let context = (&*inner) as *const PagerInner as usize;
        // SAFETY: `fault::install`'s four conditions, in order.
        //
        // * `context` is the address of a `Box` this `DemandPager` owns and never moves out of, and
        //   `registration` is declared before `inner` so it is dropped -- and the slot cleared with
        //   a release store -- before the box is freed.
        // * `handle_fault` wraps its whole body in `catch_unwind` and reports a panic as
        //   `NotOurs`, so nothing unwinds into the dispatcher.
        // * It takes this space's internal lock, and the module docs state the matching invariant:
        //   the thread running guest code never holds it. A thread-local guard breaks the
        //   single-thread version of a violation.
        // * It returns `Resolved` only when the page really is accessible afterwards: either this
        //   call committed it, or another thread committed the same granule a moment earlier. The
        //   second case is bounded to one retry per thread per address, so a disagreement between
        //   the region map and the OS becomes a typed guest fault rather than a fault loop.
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
        }
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

/// Whether `protection` permits `access`.
///
/// A write to a read-only guest page is *not* something to commit our way out of: committing it
/// would silently give the guest a permission it does not have. It is declined, and becomes a typed
/// memory-fault exit naming the address — which is what a real kernel would deliver to the guest.
fn permits(protection: Protection, access: FaultAccess) -> bool {
    match access {
        FaultAccess::Read => protection.is_readable(),
        FaultAccess::Write => protection.is_writable(),
        FaultAccess::Execute => protection.is_executable(),
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
    // `DemandPager`), and `release` clears the slot with a release store before returning, so no
    // call can begin after the box is freed.
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
        inner.declined.fetch_add(1, Ordering::Relaxed);
        return FaultOutcome::NotOurs;
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
        Err(_) => {
            inner.declined.fetch_add(1, Ordering::Relaxed);
            FaultOutcome::NotOurs
        }
    };

    IN_HANDLER.with(|flag| flag.set(false));
    outcome
}

fn resolve(inner: &PagerInner, fault: &Fault) -> FaultOutcome {
    inner.examined.fetch_add(1, Ordering::Relaxed);

    let Some(region) = inner.space.region_at(fault.address) else {
        inner.declined.fetch_add(1, Ordering::Relaxed);
        return FaultOutcome::NotOurs;
    };
    if region.is_free() || !permits(region.protection, fault.access) {
        inner.declined.fetch_add(1, Ordering::Relaxed);
        return FaultOutcome::NotOurs;
    }

    // One byte is the right request: `ensure_committed` expands outwards to whole commit granules
    // and clips to the mapping, so this commits exactly one granule of the mapping that was
    // touched, and D10's measured 150 ns/page at the 64 KiB granule is what it costs.
    match inner.space.ensure_committed(fault.address, 1) {
        Ok(0) => resolve_without_committing(inner, fault, &region),
        Ok(bytes) => {
            inner.resolved.fetch_add(1, Ordering::Relaxed);
            inner.bytes_committed.fetch_add(bytes as u64, Ordering::Relaxed);
            LAST_ZERO_COMMIT.with(|cell| cell.set(0));
            FaultOutcome::Resolved
        }
        Err(_) => {
            // Commit failed — `ERROR_COMMITMENT_LIMIT`, or this space's own ceiling (D15). Declining
            // turns it into a typed guest memory fault instead of an infinite fault loop. The error
            // is not logged here: this is an exception handler, and formatting allocates.
            inner.declined.fetch_add(1, Ordering::Relaxed);
            FaultOutcome::NotOurs
        }
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
    region: &crate::RegionInfo,
) -> FaultOutcome {
    let anonymous = matches!(region.kind, RegionKind::Anonymous);
    let repeated = LAST_ZERO_COMMIT.with(|cell| cell.replace(fault.address)) == fault.address;
    if anonymous && !repeated {
        inner.resolved.fetch_add(1, Ordering::Relaxed);
        return FaultOutcome::Resolved;
    }
    // Either file-backed — where no commit is owed and a fault means something else entirely — or
    // the same address a second time running on this thread, which means retrying did not help.
    inner.declined.fetch_add(1, Ordering::Relaxed);
    FaultOutcome::NotOurs
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let base = space.base();
        let inner = PagerInner {
            base,
            end: space.end(),
            space,
            examined: AtomicU64::new(0),
            resolved: AtomicU64::new(0),
            bytes_committed: AtomicU64::new(0),
            declined: AtomicU64::new(0),
        };
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
        assert_eq!(
            inner.declined.load(Ordering::Relaxed),
            1,
            "and it must be counted, so a pager that is faulting on itself is visible"
        );
        assert_eq!(
            inner.examined.load(Ordering::Relaxed),
            0,
            "the nested call must not have reached `resolve`, which is where the space lock is taken"
        );

        // And the flag is left clear afterwards, so one nested fault does not wedge the thread out
        // of ever serving another.
        assert!(!IN_HANDLER.with(Cell::get));
        assert_eq!(handle_fault(context, &fault), FaultOutcome::NotOurs, "no mapping there");
        assert_eq!(inner.examined.load(Ordering::Relaxed), 1, "this one did reach `resolve`");
    }

    /// The zero-commit decision, driven directly so that it is **deterministic**.
    ///
    /// The concurrency test in `omni-cpu` found this bug, but it cannot be what pins it: it depends
    /// on two guest threads faulting on the same 64 KiB granule in the same instant, which happens
    /// often enough to find a defect and not often enough to be evidence. A mutation row backed by
    /// it passed and failed on alternate runs — and Task 1's lesson is that a flaky test in a
    /// mutation table is worse than no row, because it attributes a mutation to the wrong detector.
    ///
    /// So the decision function is called directly, with the two region kinds and the repeat.
    #[test]
    fn a_commit_that_committed_nothing_is_resolved_once_for_anonymous_memory() {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let base = space.base();
        let inner = PagerInner {
            base,
            end: space.end(),
            space,
            examined: AtomicU64::new(0),
            resolved: AtomicU64::new(0),
            bytes_committed: AtomicU64::new(0),
            declined: AtomicU64::new(0),
        };
        let fault = Fault {
            address: base + 0x2000,
            access: FaultAccess::Write,
            instruction_pointer: 0,
        };
        let region = |kind| crate::RegionInfo {
            start: base,
            len: 0x10000,
            protection: Protection::ReadWrite,
            kind,
            committed: 0x10000,
            mapping: None,
            mapping_start: base,
            mapping_len: 0x10000,
        };

        LAST_ZERO_COMMIT.with(|cell| cell.set(0));

        // Anonymous, first time at this address: another thread committed the granule a moment ago,
        // so the page IS accessible and retrying the instruction succeeds. Declining here is what
        // handed the fault to dynarmic and put the block permanently on the callback path.
        assert_eq!(
            resolve_without_committing(&inner, &fault, &region(RegionKind::Anonymous)),
            FaultOutcome::Resolved
        );
        assert_eq!(inner.resolved.load(Ordering::Relaxed), 1);

        // Same address again on this thread: retrying did not help, so it becomes a typed guest
        // fault rather than a loop. This is the bound on the one case that could otherwise spin.
        assert_eq!(
            resolve_without_committing(&inner, &fault, &region(RegionKind::Anonymous)),
            FaultOutcome::NotOurs
        );
        assert_eq!(inner.declined.load(Ordering::Relaxed), 1);
        assert_eq!(inner.resolved.load(Ordering::Relaxed), 1, "and it is not counted twice");

        // File-backed: no commit is owed, so a fault there means something this pager cannot fix.
        LAST_ZERO_COMMIT.with(|cell| cell.set(0));
        let file = RegionKind::File {
            backing: crate::BackingId(1),
            name: "libroblox.so".into(),
            file_offset: 0,
        };
        assert_eq!(
            resolve_without_committing(&inner, &fault, &region(file)),
            FaultOutcome::NotOurs
        );
        assert_eq!(inner.declined.load(Ordering::Relaxed), 2);
    }

    /// The permission table, pinned. Getting `Write` wrong here would make the pager commit a page
    /// the guest is not allowed to write, which is a silently granted permission rather than a
    /// crash — the exact shape Global Constraint 11 warns about.
    #[test]
    fn a_fault_is_only_ours_if_the_protection_already_allowed_it() {
        use FaultAccess::{Execute, Read, Write};
        for (protection, read, write, exec) in [
            (Protection::None, false, false, false),
            (Protection::Read, true, false, false),
            (Protection::ReadWrite, true, true, false),
            (Protection::ReadExecute, true, false, true),
        ] {
            assert_eq!(permits(protection, Read), read, "{protection} read");
            assert_eq!(permits(protection, Write), write, "{protection} write");
            assert_eq!(permits(protection, Execute), exec, "{protection} execute");
        }
    }
}
