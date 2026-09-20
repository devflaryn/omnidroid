//! **The one place that decides whether guest code may touch an address.**
//!
//! # Why this module exists
//!
//! It did not, and the whole-branch review found the consequence: two independent answers to the
//! same question, on opposite sides of a crate boundary, with **different rules**.
//! `CpuCtx::resolve` in `omni-cpu` checked the guest extent and the *full length* of the access and
//! committed only for [`RegionKind::Anonymous`]; `permits`/`resolve` in this crate's `pager` checked
//! neither length nor kind and called `ensure_committed` for anything. Both were defensible and
//! neither was reachable from the other, so a rule added to one would simply not arrive at the
//! other — and no per-task review could see it, because each saw one side.
//!
//! So the policy is written once, here, and both callers ask it. What they do with the answer still
//! differs, and should: the CPU's slow-path callback turns a refusal into a typed `ExitReason`, and
//! the pager turns it into "not ours", which lets normal exception dispatch continue.
//!
//! # The rules, in order
//!
//! 1. **Mapped.** Some region covers the address, and it is not free space.
//! 2. **Whole.** The access does not run off the end of that region.
//! 3. **Permitted.** The region's protection already allows this access. A write to a read-only guest
//!    page is *not* something to commit our way out of: committing it would hand the guest a
//!    permission it does not have, which is Global Constraint 11's "saturating arithmetic on a limit
//!    turns hostile input into a larger permission" in another costume. It is refused, and becomes a
//!    typed fault naming the address — which is what a real kernel would deliver.
//! 4. **Accessible.** If the region is anonymous and not yet fully committed, commit the granule
//!    covering the access. Nothing else is ever committed: a file-backed view owes no commit charge
//!    (D10), so a fault on one means something this policy cannot fix.
//!
//! # The one deliberate divergence, and why it is not a bug
//!
//! Rule 2 is a no-op for the demand pager, because it always asks about **one byte**. That is not
//! laziness, it is what the hardware reports: an access violation names the address that could not be
//! reached, and an access straddling a page boundary faults separately on the second page. Asking the
//! pager to validate a length it was never told would be inventing one. The CPU's callback *is* told
//! the length, because dynarmic hands it over, so it checks it — and the two are the same rule
//! applied to the information each side actually has.
//!
//! # What is deliberately *not* here
//!
//! The guest address-space extent. `omni-cpu` checks `GuestAddressSpace::contains` before calling,
//! which is a cheap reject on an address that cannot possibly be mapped; rule 1 would refuse the same
//! address a moment later, from the region map, which is authoritative. Duplicating the extent here
//! would put the same number in two places again.

use omni_platform::fault::FaultAccess;

use crate::space::{GuestAddr, GuestSpace};
use crate::{Protection, RegionInfo, RegionKind};

/// Whether `protection` already permits `access`.
///
/// Small enough to inline and important enough to name: getting `Write` wrong here makes the pager
/// commit a page the guest is not allowed to write, which is a silently granted permission rather
/// than a crash.
#[must_use]
pub fn permits(protection: Protection, access: FaultAccess) -> bool {
    match access {
        FaultAccess::Read => protection.is_readable(),
        FaultAccess::Write => protection.is_writable(),
        FaultAccess::Execute => protection.is_executable(),
    }
}

/// Why [`admit`] said no.
///
/// Three reasons rather than one boolean, because the callers report them differently and because a
/// refusal that cannot say which rule it failed is a refusal nobody can act on (Global Constraint 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Nothing is mapped at the address, or the access runs off the end of the region that is
    /// (rules 1 and 2).
    NotMapped,
    /// A region is there, but its protection does not allow this access (rule 3).
    Protection,
    /// The granule could not be committed — the system commit limit, or this space's own ceiling
    /// (D15). Rule 4.
    Commit,
}

/// What [`admit`] found, when it said yes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admitted {
    /// Start of the region the address falls in.
    pub start: GuestAddr,
    /// End of that region, exclusive.
    pub end: GuestAddr,
    /// Bytes this call committed. **Zero is not an error**: it means nothing was owed, or another
    /// thread committed the same granule a moment earlier. The pager has to tell those apart; see
    /// `pager::resolve_without_committing` for the concurrency bug that taught it to.
    pub committed: usize,
    /// Whether the whole region was already committed when this call started.
    ///
    /// The only caller that needs it is `omni-cpu`'s instruction fetch, which caches a region's
    /// extent and skips re-asking for addresses inside it. That is only sound while there is nothing
    /// left to commit, so this is the flag that says whether caching is allowed.
    pub fully_committed: bool,
    /// Whether the region is anonymous — the only kind rule 4 will commit for.
    pub anonymous: bool,
}

/// Decide whether guest code may access `[address, address + len)` for `access`, committing the
/// granule if that is the only thing in the way.
///
/// See the module docs for the rules and for the one place the two callers legitimately differ.
///
/// # Errors
///
/// [`Refusal`], naming the rule that failed.
pub fn admit(
    space: &GuestSpace,
    address: GuestAddr,
    len: usize,
    access: FaultAccess,
) -> Result<Admitted, Refusal> {
    // Rule 1, first half: something has to be there at all.
    let Some(region) = space.region_at(address) else {
        return Err(Refusal::NotMapped);
    };
    let Some(access_end) = address.checked_add(len.max(1)) else {
        return Err(Refusal::NotMapped);
    };

    // Rules 1-3, over every entry the access touches.
    //
    // **An access may span entries, provided they are one mapping.** A commit carves the map into
    // entries that are each exactly one OS placeholder -- `commit_range` requires that, because a
    // commit may not cross a placeholder -- and adjacent committed granules are never coalesced back
    // together. So a lazily-committed mapping ends up as a run of entries, and an access that
    // straddles a granule boundary lands in two of them while being, to the guest, one perfectly
    // ordinary access inside one `mmap`.
    //
    // Checking only the first entry refused every such access as `NotMapped` even when both granules
    // were committed. MEASURED: on a 1 MiB lazy mapping with two neighbouring granules committed, an
    // 8-byte read inside the first granule succeeded and a 16-byte read straddling the boundary was
    // refused. Every existing fixture is `CommitPolicy::Eager`, which is one entry that is never
    // split, which is why nothing saw it.
    //
    // The walk stops at the mapping, not at the entry: crossing into a *different* mapping is a real
    // refusal, and so is crossing free space.
    let anonymous = matches!(region.kind, RegionKind::Anonymous);
    let mut covered_end = region.end();
    let mut fully_committed = region.is_committed();
    admits_region(&region, address, access_end.min(covered_end) - address, access)?;

    while covered_end < access_end {
        let Some(next) = space.region_at(covered_end) else {
            return Err(Refusal::NotMapped);
        };
        // Contiguous, and the same mapping. `mapping` is `None` for free space, so a `None == None`
        // comparison must not be allowed to pass for two unrelated holes.
        if next.start != covered_end || next.mapping.is_none() || next.mapping != region.mapping {
            return Err(Refusal::NotMapped);
        }
        admits_region(&next, covered_end, access_end.min(next.end()) - covered_end, access)?;
        fully_committed &= next.is_committed();
        covered_end = next.end();
    }

    // Rule 4.
    let committed = if anonymous && !fully_committed {
        // One byte would do for a fault, but the CPU callback knows the real length and a commit
        // that covered only the first byte of a straddling access would fault again immediately.
        // `ensure_committed` expands outwards to whole granules and clips to the mapping, and
        // `commit_range` already walks entries, so a straddling commit is one call.
        match space.ensure_committed(address, len.max(1)) {
            Ok(bytes) => bytes,
            Err(_) => return Err(Refusal::Commit),
        }
    } else {
        0
    };

    Ok(Admitted {
        start: region.start,
        // The end of the last entry needed to cover the access, which for an access inside one entry
        // is that entry's end exactly as before. `fully_committed` is the AND over those entries, so
        // `omni-cpu`'s fetch cache keeps its contract: it only trusts `end` when that flag is set.
        end: covered_end,
        committed,
        fully_committed,
        anonymous,
    })
}

/// Rules 1-3 of [`admit`] — mapped, whole, permitted — for a caller that already has the
/// [`RegionInfo`].
///
/// [`admit`] calls this rather than repeating it, so these three rules exist exactly once in the
/// workspace. That is the property M4 was about: with two copies, a mutation row could flip a rule on
/// one side and the other side would not notice, because the other side had its own.
///
/// It is also what lets the boundary cases be driven directly, with a `RegionInfo` a test built,
/// instead of by arranging a live mapping whose exact extent the test then has to reason about.
///
/// # Errors
///
/// [`Refusal`], naming the rule that failed.
pub fn admits_region(
    region: &RegionInfo,
    address: GuestAddr,
    len: usize,
    access: FaultAccess,
) -> Result<(), Refusal> {
    if region.is_free() {
        return Err(Refusal::NotMapped);
    }
    let Some(access_end) = address.checked_add(len) else {
        return Err(Refusal::NotMapped);
    };
    if access_end > region.end() {
        return Err(Refusal::NotMapped);
    }
    if !permits(region.protection, access) {
        return Err(Refusal::Protection);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The permission table, pinned over every protection and every access.
    ///
    /// This is the rule both crates now share, so getting it wrong is wrong in two places at once —
    /// which is the argument for having one copy, and the reason this table is exhaustive.
    #[test]
    fn a_protection_permits_exactly_the_accesses_it_names() {
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
        // Every variant was covered, so a new one cannot be added without failing here.
        assert_eq!(Protection::ALL.len(), 4, "Protection gained or lost a variant");
    }

    /// Rule 2, which is the rule the two sides used to disagree about.
    ///
    /// The pager asks about one byte and can never trip this; the CPU callback is told the length by
    /// dynarmic and must. Driven through `admits_region` so the boundary can be walked exactly.
    #[test]
    fn an_access_may_not_run_off_the_end_of_its_region() {
        let region = RegionInfo {
            start: 0x1_0000,
            len: 0x1000,
            protection: Protection::ReadWrite,
            kind: RegionKind::Anonymous,
            committed: 0x1000,
            mapping: None,
            mapping_start: 0x1_0000,
            mapping_len: 0x1000,
        };
        let last = region.end() - 1;

        // One byte at the last byte is inside; two bytes there is not.
        assert_eq!(admits_region(&region, last, 1, FaultAccess::Read), Ok(()));
        assert_eq!(
            admits_region(&region, last, 2, FaultAccess::Read),
            Err(Refusal::NotMapped),
            "an access whose tail leaves the region is refused, not truncated"
        );
        // The exact fit, which is the off-by-one worth having a case for.
        assert_eq!(admits_region(&region, region.start, region.len, FaultAccess::Read), Ok(()));
        assert_eq!(
            admits_region(&region, region.start, region.len + 1, FaultAccess::Read),
            Err(Refusal::NotMapped)
        );
        // A length that overflows the address space is refused rather than wrapping into range: it
        // arrives from generated guest code (Global Constraint 11).
        assert_eq!(
            admits_region(&region, last, usize::MAX, FaultAccess::Read),
            Err(Refusal::NotMapped)
        );

        // And protection is checked after extent, so a refusal names the first rule that failed.
        assert_eq!(
            admits_region(&region, last, 1, FaultAccess::Execute),
            Err(Refusal::Protection)
        );
        assert_eq!(
            admits_region(&region, last, 2, FaultAccess::Execute),
            Err(Refusal::NotMapped),
            "extent is checked first, so this is NotMapped rather than Protection"
        );
    }

    /// Free space is refused whatever the access, and whatever protection the entry happens to carry.
    #[test]
    fn free_address_space_admits_nothing() {
        let free = RegionInfo {
            start: 0x1_0000,
            len: 0x1000,
            // Deliberately a permissive protection on a free region, which the region map never
            // produces, so that the `is_free` check is what refuses rather than the protection.
            protection: Protection::ReadWrite,
            kind: RegionKind::Free,
            committed: 0,
            mapping: None,
            mapping_start: 0x1_0000,
            mapping_len: 0x1000,
        };
        for access in [FaultAccess::Read, FaultAccess::Write, FaultAccess::Execute] {
            assert_eq!(
                admits_region(&free, free.start, 1, access),
                Err(Refusal::NotMapped),
                "{access}"
            );
        }
    }
}
