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
//!    and that permanently deoptimizes the block onto the callback path (`recompile_on_fastmem_
//!    failure`) — a **13.2x** slower path (D4), taken silently, with correct results. D10 requires
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
use crate::Protection;

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
        let registration = fault::install(handle_fault, context)?;
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

    let outcome = catch_unwind(AssertUnwindSafe(|| resolve(inner, fault)))
        .unwrap_or(FaultOutcome::NotOurs);

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
        Ok(0) => {
            // The range was already committed, or is file-backed, or is a `Protection::None`
            // mapping. In every one of those cases the fault was not a missing commit, so nothing
            // here can fix it and continuing execution would fault again immediately.
            inner.declined.fetch_add(1, Ordering::Relaxed);
            FaultOutcome::NotOurs
        }
        Ok(bytes) => {
            inner.resolved.fetch_add(1, Ordering::Relaxed);
            inner.bytes_committed.fetch_add(bytes as u64, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

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
