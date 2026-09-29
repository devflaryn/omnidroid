//! The 4 KiB overlay applied to a [`GuestSpace`] (`crate::subpage` has what a page's parts mean;
//! spec `2026-09-29-4k-guest-pages`). A child of `space` so that it uses the space's own map
//! operations rather than a second copy of them.
//!
//! **SUBPAGE-ORDER** (the lock and publication rule every path here keeps):
//! 1. Lock order: `omni-linux`'s layout lock → the space's `inner` → the overlay's `state`.
//!    `state` is taken only while `inner` is held, except by the report and the served counter,
//!    which use `try_lock`.
//! 2. The fault path (the pager, and [`GuestSpace::access_ptr`] in the slow path) reads only the
//!    atomic count and bits before deciding; it takes no lock before `admit`, which takes `inner`.
//! 3. A page starts trapping as: alias live → bit set (`Release`) → host protection lowered. It
//!    stops as: host protection raised → bit cleared.
//! 4. Parts are written only under `inner`; a reader holds `inner` or a per-thread cache entry
//!    validated by the generation that `write()` bumps before any change.
//! 5. An alias is unmapped only after its host page is unmapped or replaced, never on a
//!    protection change.

use super::{GuestAddr, GuestSpace, Inner, OsState, Placement};
use crate::error::{platform, MemError, MemResult};
use crate::region::{RegionInfo, RegionKind};
use crate::subpage::{Part, Split, SubPages, SubPagesHandle, GUEST_PAGE};
use crate::CommitPolicy;
use omni_platform::vm::{self, Protection};

/// What the guest asked of a 4 KiB-exact range.
#[derive(Debug, Clone, Copy)]
pub(super) enum SubOp {
    Map(Protection, CommitPolicy),
    Protect(Protection),
    Unmap,
}

/// Where an admitted access should go ([`GuestSpace::access_ptr`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessPtr {
    /// The ordinary host address: no trapping host page is touched.
    Direct(*mut u8),
    /// Every byte is in trapping host pages: their read-write alias.
    Alias(*mut u8),
    /// The range mixes trapping and ordinary host pages: copy it in chunks
    /// ([`GuestSpace::for_each_access_chunk`]).
    Straddle,
}

/// The overlay's numbers, for the memory report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitStats {
    /// Host pages the overlay tracks.
    pub tracked: usize,
    /// Tracked host pages that trap (have a live alias and a set bit).
    pub trapping: usize,
    /// Accesses the slow path served through an alias, in all.
    pub served_total: u64,
    /// The host pages served most, with their counts, most first (at most eight).
    pub top: Vec<(GuestAddr, u64)>,
}

impl GuestSpace {
    fn sub_handle(&self) -> &SubPagesHandle {
        self.sub.as_ref().expect("the overlay is active")
    }

    /// Host pages in the space.
    fn host_pages(&self) -> usize {
        self.len / self.page
    }

    /// Whether `[address, address + len)` must take the overlay path: the space keeps one, and the
    /// range is not whole host pages or touches a tracked one.
    pub(super) fn needs_overlay(&self, inner: &Inner, address: GuestAddr, len: usize) -> bool {
        let _ = inner; // held: SUBPAGE-ORDER 1
        let Some(sub) = &self.sub else { return false };
        let page = self.page;
        if address % page != 0 || len % page != 0 {
            return true;
        }
        let st = sub.state.lock();
        st.split.range(address & !(page - 1)..address + len).next().is_some()
    }

    /// Check a 4 KiB-exact range against the overlay's alignment rules.
    pub(super) fn check_guest_range(&self, operation: &'static str, address: GuestAddr, len: usize) -> MemResult<usize> {
        if len == 0 {
            return Err(MemError::ZeroSize { operation });
        }
        if address % GUEST_PAGE != 0 {
            return Err(MemError::Misaligned { operation, what: "address", value: address as u64, required: GUEST_PAGE as u64 });
        }
        let len = len.checked_add(GUEST_PAGE - 1).map(|l| l & !(GUEST_PAGE - 1)).ok_or(MemError::ZeroSize { operation })?;
        self.check_range(operation, address, len)?;
        Ok(len)
    }

    /// Whether every part of `[address, address + len)` is mapped (`want_mapped`) or every part is
    /// free, in the guest's view. Nothing is changed.
    fn guest_view_is(&self, inner: &Inner, st: &SubPages, address: GuestAddr, len: usize, want_mapped: bool) -> bool {
        let page = self.page;
        let end = address + len;
        let mut at = address;
        while at < end {
            let host_page = at & !(page - 1);
            let next = (host_page + page).min(end);
            if let Some(s) = st.split.get(&host_page) {
                let (i0, i1) = ((at - host_page) / GUEST_PAGE, (next - host_page).div_ceil(GUEST_PAGE));
                let ok = s.parts[i0..i1].iter().all(|p| matches!(p, Part::Mapped(_)) == want_mapped);
                if !ok {
                    return false;
                }
            } else {
                let ok = if want_mapped {
                    inner.require_mapped("guest view", at, next - at).is_ok()
                } else {
                    inner.require_free("guest view", at, next - at).is_ok()
                };
                if !ok {
                    return false;
                }
            }
            at = next;
        }
        true
    }

    /// The overlay path for a 4 KiB-exact map ([`Placement::Fixed`] only), protect or unmap.
    /// `inner` is held (it bumped the generation, SUBPAGE-ORDER 4).
    pub(super) fn sub_apply(&self, inner: &mut Inner, operation: &'static str, address: GuestAddr, len: usize, op: SubOp) -> MemResult<()> {
        let sub = self.sub_handle();
        let mut st = sub.state.lock();
        // Validate the whole range before changing any of it, as Linux does: an `mprotect` over a
        // hole is `ENOMEM` and a fixed map over something is refused, with nothing changed.
        match op {
            SubOp::Map(..) if !self.guest_view_is(inner, &st, address, len, false) => {
                return Err(MemError::AddressTaken {
                    operation,
                    requested: address,
                    requested_end: address + len,
                    conflict_start: address,
                    conflict_end: address + len,
                    conflict: "a mapped 4 KiB page".into(),
                });
            }
            SubOp::Protect(_) if !self.guest_view_is(inner, &st, address, len, true) => {
                return Err(MemError::NotMapped { operation, address, end: address + len, unmapped_start: address, unmapped_end: address + len });
            }
            _ => {}
        }
        let split = super::split_at_pages(address, len, self.page);
        if let Some((whole, whole_len)) = split.whole {
            self.apply_whole(inner, &mut st, operation, whole, whole_len, op)?;
        }
        for (at, part_len) in split.partial() {
            self.apply_partial(inner, &mut st, operation, at, part_len, op)?;
        }
        Ok(())
    }

    /// Whole host pages: today's path, after the tracked pages among them leave the overlay.
    fn apply_whole(&self, inner: &mut Inner, st: &mut SubPages, operation: &'static str, address: GuestAddr, len: usize, op: SubOp) -> MemResult<()> {
        let tracked: Vec<usize> = st.split.range(address..address + len).map(|(p, _)| *p).collect();
        match op {
            SubOp::Map(protection, commit) => {
                self.map_anonymous_locked(inner, operation, Placement::Fixed(address), len, protection, commit)?;
            }
            SubOp::Protect(protection) => {
                inner.require_mapped(operation, address, len)?;
                // Host first, then the bits (SUBPAGE-ORDER 3).
                inner.protect_range(operation, address, len, protection)?;
            }
            SubOp::Unmap => {
                inner.unmap_range(operation, address, len)?;
            }
        }
        for host_page in tracked {
            st.split.remove(&host_page);
            self.sub_handle().set_trapping((host_page - self.base) / self.page, self.host_pages(), false);
        }
        if matches!(op, SubOp::Unmap) {
            self.unalias_range(st, address, len);
        }
        Ok(())
    }

    /// A part of one host page.
    fn apply_partial(&self, inner: &mut Inner, st: &mut SubPages, operation: &'static str, at: GuestAddr, len: usize, op: SubOp) -> MemResult<()> {
        let page = self.page;
        let host_page = at & !(page - 1);
        let (i0, i1) = ((at - host_page) / GUEST_PAGE, (at + len - host_page) / GUEST_PAGE);
        let mut split = match st.split.get(&host_page) {
            Some(s) => s.clone(),
            None => {
                let entry = inner.map.entry_start(host_page).and_then(|s| inner.map.get(s).map(|e| (s, e.clone())));
                let mapped = entry.as_ref().filter(|(_, e)| !e.is_free()).map(|(s, e)| RegionInfo::from_entry(*s, e).protection);
                match (mapped, op) {
                    (Some(protection), _) => Split::new(st.parts_per_page, Part::Mapped(protection)),
                    (None, SubOp::Map(..)) => {
                        // A free host page gets a mapping of its own; its other parts are holes.
                        self.map_anonymous_locked(inner, operation, Placement::Fixed(host_page), page, Protection::None, CommitPolicy::Lazy)?;
                        Split::new(st.parts_per_page, Part::Hole)
                    }
                    (None, SubOp::Unmap) => return Ok(()),
                    (None, SubOp::Protect(_)) => unreachable!("validated: protect over a hole is refused"),
                }
            }
        };
        let (fill, zero, eager) = match op {
            SubOp::Map(protection, commit) => (Part::Mapped(protection), true, commit == CommitPolicy::Eager),
            SubOp::Protect(protection) => (Part::Mapped(protection), false, false),
            SubOp::Unmap => (if Self::is_strict(st, at, len) { Part::StrictHole } else { Part::Hole }, false, false),
        };
        split.set(i0, i1, fill);
        self.settle(inner, st, operation, host_page, split, zero.then_some((i0, i1)), eager)
    }

    fn is_strict(st: &SubPages, at: GuestAddr, len: usize) -> bool {
        st.strict.iter().any(|&(s, e)| s < at + len && at < e)
    }

    /// Put a changed host page's parts into effect, in SUBPAGE-ORDER. `zero`: parts newly mapped,
    /// which must read zero.
    fn settle(
        &self,
        inner: &mut Inner,
        st: &mut SubPages,
        operation: &'static str,
        host_page: GuestAddr,
        split: Split,
        zero: Option<(usize, usize)>,
        eager: bool,
    ) -> MemResult<()> {
        let sub = self.sub_handle();
        let page = self.page;
        let index = (host_page - self.base) / page;
        let committed = |inner: &Inner| {
            inner.map.entry_start(host_page).and_then(|s| inner.map.get(s)).is_some_and(|e| matches!(e.os, OsState::Private { .. } | OsState::View { .. }))
        };
        if split.empty() {
            sub.set_trapping(index, self.host_pages(), false);
            st.split.remove(&host_page);
            inner.unmap_range(operation, host_page, page)?;
            self.unalias_range(st, host_page, page);
            return Ok(());
        }
        let needs_view = split.traps() || eager || (zero.is_some() && committed(inner));
        if needs_view {
            self.commit_page(inner, operation, host_page)?;
            self.privatise(inner, st, operation, host_page)?;
            self.ensure_alias(st, host_page)?;
        }
        if let Some((i0, i1)) = zero {
            if committed(inner) {
                let alias = self.alias_of(st, host_page);
                // SAFETY: the alias is a live read-write view of this committed host page (just
                // made), and the parts being zeroed are ones the guest has just mapped afresh.
                unsafe { std::ptr::write_bytes((alias + i0 * GUEST_PAGE) as *mut u8, 0, (i1 - i0) * GUEST_PAGE) };
            }
        }
        if let Some(protection) = split.uniform() {
            // Leaves the overlay: host first, then the bit (the alias stays, SUBPAGE-ORDER 5).
            inner.protect_range(operation, host_page, page, protection)?;
            sub.set_trapping(index, self.host_pages(), false);
            st.split.remove(&host_page);
            return Ok(());
        }
        let host = split.host_protection();
        if split.traps() {
            sub.set_trapping(index, self.host_pages(), true);
            inner.protect_range(operation, host_page, page, host)?;
        } else {
            inner.protect_range(operation, host_page, page, host)?;
            sub.set_trapping(index, self.host_pages(), false);
        }
        st.split.insert(host_page, split);
        Ok(())
    }

    /// Commit one host page whatever protection its entry records (`commit_range` skips a `None`
    /// one): an alias needs memory behind it.
    fn commit_page(&self, inner: &mut Inner, operation: &'static str, host_page: GuestAddr) -> MemResult<()> {
        let page = self.page;
        let Some(start) = inner.map.entry_start(host_page) else { return Ok(()) };
        let entry = inner.map.get(start).expect("entry vanished");
        if entry.os != OsState::Placeholder {
            return Ok(());
        }
        let recorded = entry.owner.as_ref().map_or(Protection::None, |o| o.protection);
        inner.check_commit_allowed(operation, host_page, page)?;
        inner.make_exact_placeholder(operation, host_page, page, false)?;
        let with = if recorded == Protection::None { Protection::Read } else { recorded };
        // SAFETY: `[host_page, +page)` is now exactly one unreplaced placeholder piece this space
        // owns, and nothing references it (a placeholder is inaccessible).
        unsafe { vm::commit_placeholder(inner.host(host_page) as *mut u8, page, with) }.map_err(platform(operation, host_page, page))?;
        if with != recorded {
            // SAFETY: just committed, this space's; a protection change dereferences nothing.
            unsafe { vm::protect(inner.host(host_page) as *mut u8, page, recorded) }.map_err(platform(operation, host_page, page))?;
        }
        inner.map.get_mut(host_page).expect("entry vanished").os = OsState::Private { idle: false };
        inner.committed += page;
        Ok(())
    }

    /// A file view's host page becomes private anonymous memory with the same bytes, so it can be
    /// aliased without depending on how the host shares a copy-on-write view.
    fn privatise(&self, inner: &mut Inner, st: &mut SubPages, operation: &'static str, host_page: GuestAddr) -> MemResult<()> {
        let page = self.page;
        let Some(start) = inner.map.entry_start(host_page) else { return Ok(()) };
        let entry = inner.map.get(start).expect("entry vanished");
        if !matches!(entry.os, OsState::View { .. }) {
            return Ok(());
        }
        let recorded = entry.owner.as_ref().map_or(Protection::Read, |o| o.protection);
        if !recorded.is_readable() {
            inner.protect_range(operation, host_page, page, Protection::Read)?;
        }
        let mut bytes = vec![0u8; page];
        // SAFETY: the page is a live view this space owns, readable now; nothing writes it while
        // `inner` is held but guest code, whose writes the copy may or may not see -- as a
        // concurrent write during the guest's own mprotect may or may not land.
        unsafe { std::ptr::copy_nonoverlapping(inner.host(host_page) as *const u8, bytes.as_mut_ptr(), page) };
        inner.unmap_range(operation, host_page, page)?;
        self.unalias_range(st, host_page, page);
        self.map_anonymous_locked(inner, operation, Placement::Fixed(host_page), page, Protection::ReadWrite, CommitPolicy::Eager)?;
        // SAFETY: just mapped, committed and read-write.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), inner.host(host_page) as *mut u8, page) };
        inner.protect_range(operation, host_page, page, recorded)?;
        Ok(())
    }

    fn alias_of(&self, st: &SubPages, host_page: GuestAddr) -> usize {
        st.alias.as_ref().expect("an alias reservation").base() + (host_page - self.base)
    }

    /// Make (or remake) host page `host_page`'s alias. Remade every time a page is made to trap, so
    /// an alias left from before a decommit never serves stale memory.
    fn ensure_alias(&self, st: &mut SubPages, host_page: GuestAddr) -> MemResult<()> {
        if st.alias.is_none() {
            let reservation = vm::reserve(self.len, self.page).map_err(platform("alias reservation", 0, self.len))?;
            self.sub_handle().alias_base.store(reservation.base(), std::sync::atomic::Ordering::Release);
            st.alias = Some(reservation);
        }
        let dst = self.alias_of(st, host_page);
        // SAFETY: the source is committed private memory of this space (`commit_page`,
        // `privatise`); the destination is inside the alias reservation, which nothing else uses.
        unsafe { vm::alias(self.host_addr(host_page) as *mut u8, dst as *mut u8, self.page) }
            .map_err(platform("alias", host_page, self.page))?;
        st.aliased.insert(host_page);
        Ok(())
    }

    /// Unmap the aliases of host pages in `[address, address + len)`, after those pages were
    /// unmapped or replaced (SUBPAGE-ORDER 5).
    pub(super) fn unalias_range(&self, st: &mut SubPages, address: GuestAddr, len: usize) {
        let gone: Vec<usize> = st.aliased.range(address..address + len).copied().collect();
        for host_page in gone {
            st.aliased.remove(&host_page);
            let dst = self.alias_of(st, host_page);
            // SAFETY: an alias this overlay made; its page is gone, so nothing may reach it.
            if let Err(e) = unsafe { vm::unalias(dst as *mut u8, self.page) } {
                tracing::error!(%e, host_page = format_args!("{host_page:#x}"), "an alias could not be unmapped");
            }
        }
    }

    /// Unmap aliases in a range the caller already unmapped or decommitted by the host path, when
    /// the space keeps an overlay. Takes the overlay's lock: `inner` must be held.
    pub(super) fn forget_aliases(&self, address: GuestAddr, len: usize) {
        if let Some(sub) = &self.sub {
            let mut st = sub.state.lock();
            if !st.aliased.is_empty() {
                self.unalias_range(&mut st, address, len);
            }
        }
    }

    /// `region_at`'s answer in the guest's view: in a tracked page, the run of parts at `address`
    /// (none for a hole); elsewhere, clipped so that it covers no tracked page. `inner` is held.
    pub(super) fn guest_view_of(&self, info: RegionInfo, address: GuestAddr) -> Option<RegionInfo> {
        let Some(sub) = &self.sub else { return Some(info) };
        let st = sub.state.lock();
        if st.split.is_empty() {
            return Some(info);
        }
        let page = self.page;
        let host_page = address & !(page - 1);
        if let Some(split) = st.split.get(&host_page) {
            let (first, count, part) = split.run_at((address - host_page) / GUEST_PAGE);
            let Part::Mapped(protection) = part else { return None };
            return Some(RegionInfo {
                start: host_page + first * GUEST_PAGE,
                len: count * GUEST_PAGE,
                protection,
                committed: if info.committed > 0 { count * GUEST_PAGE } else { 0 },
                ..info
            });
        }
        let lo = st.split.range(..host_page).next_back().map_or(info.start, |(p, _)| (p + page).max(info.start));
        let hi = st.split.range(host_page..).next().map_or(info.end(), |(p, _)| (*p).min(info.end()));
        Some(RegionInfo { start: lo, len: hi - lo, committed: if info.is_committed() { hi - lo } else { 0 }, ..info })
    }

    /// Expand tracked pages in `regions` (address order) into their runs; holes are free regions
    /// when `include_free`, else left out. `inner` is held.
    pub(super) fn guest_view_regions(&self, regions: Vec<RegionInfo>, include_free: bool) -> Vec<RegionInfo> {
        let Some(sub) = &self.sub else { return regions };
        let st = sub.state.lock();
        if st.split.is_empty() {
            return regions;
        }
        expand(regions, &st.split, self.page, include_free)
    }

    /// `[address, address + len)` cut at host page boundaries into maximal pieces that are all
    /// tracked or all not: `(start, len, tracked)`. `inner` is held.
    pub(super) fn pieces_by_tracking(&self, inner: &Inner, address: GuestAddr, len: usize) -> Vec<(GuestAddr, usize, bool)> {
        let _ = inner; // held: SUBPAGE-ORDER 1
        let Some(sub) = &self.sub else { return vec![(address, len, false)] };
        let st = sub.state.lock();
        let page = self.page;
        let end = address + len;
        let mut out: Vec<(GuestAddr, usize, bool)> = Vec::new();
        let mut at = address;
        while at < end {
            let next = ((at & !(page - 1)) + page).min(end);
            let tracked = st.split.contains_key(&(at & !(page - 1)));
            match out.last_mut() {
                Some(last) if last.2 == tracked && last.0 + last.1 == at => last.1 += next - at,
                _ => out.push((at, next - at, tracked)),
            }
            at = next;
        }
        out
    }

    /// Zero the mapped parts of `[at, at + len)`, inside tracked host pages, through their alias
    /// (`discard`). Returns the bytes zeroed.
    pub(super) fn zero_tracked(&self, inner: &mut Inner, operation: &'static str, at: GuestAddr, len: usize) -> MemResult<usize> {
        let sub = self.sub_handle();
        let mut st = sub.state.lock();
        let page = self.page;
        let mut zeroed = 0;
        let mut p = at & !(page - 1);
        while p < at + len {
            let committed = inner.map.entry_start(p).and_then(|s| inner.map.get(s)).is_some_and(|e| matches!(e.os, OsState::Private { .. } | OsState::View { .. }));
            if committed {
                self.privatise(inner, &mut st, operation, p)?;
                self.ensure_alias(&mut st, p)?;
                let (from, to) = (at.max(p), (at + len).min(p + page));
                let mapped: Vec<(usize, usize)> = {
                    let s = st.split.get(&p).expect("tracked");
                    (from..to).step_by(GUEST_PAGE).filter(|g| matches!(s.parts[(g - p) / GUEST_PAGE], Part::Mapped(_))).map(|g| (g, GUEST_PAGE.min(to - g))).collect()
                };
                for (g, n) in mapped {
                    // SAFETY: the alias is a live read-write view of this committed page.
                    unsafe { std::ptr::write_bytes((self.alias_of(&st, p) + (g - p)) as *mut u8, 0, n) };
                    zeroed += n;
                }
            }
            p += page;
        }
        Ok(zeroed)
    }

    /// Write `bytes` at `at`, inside tracked host pages, through their alias whatever the parts
    /// allow (`write_forced`). Every part written must be mapped.
    pub(super) fn write_tracked(&self, inner: &mut Inner, operation: &'static str, at: GuestAddr, bytes: &[u8]) -> MemResult<()> {
        let sub = self.sub_handle();
        let mut st = sub.state.lock();
        if !self.guest_view_is(inner, &st, at & !(GUEST_PAGE - 1), (at + bytes.len()).next_multiple_of(GUEST_PAGE) - (at & !(GUEST_PAGE - 1)), true) {
            return Err(MemError::NotMapped { operation, address: at, end: at + bytes.len(), unmapped_start: at, unmapped_end: at + bytes.len() });
        }
        let page = self.page;
        let mut p = at & !(page - 1);
        while p < at + bytes.len() {
            self.commit_page(inner, operation, p)?;
            self.privatise(inner, &mut st, operation, p)?;
            self.ensure_alias(&mut st, p)?;
            p += page;
        }
        // SAFETY: every host page of the range is committed and has a live read-write alias, and
        // the alias reservation is contiguous in the space's order.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.alias_of(&st, at & !(page - 1)).wrapping_add(at % page) as *mut u8, bytes.len()) };
        Ok(())
    }

    /// Whether any tracked part of `[at, at + len)` is executable in the guest's view.
    pub(super) fn any_executable_part(&self, at: GuestAddr, len: usize) -> bool {
        let Some(sub) = &self.sub else { return false };
        let st = sub.state.lock();
        let page = self.page;
        let end = at.saturating_add(len);
        st.split.range(at & !(page - 1)..end).any(|(p, s)| {
            s.parts.iter().enumerate().any(|(i, part)| {
                let (ps, pe) = (p + i * GUEST_PAGE, p + (i + 1) * GUEST_PAGE);
                ps < end && at < pe && matches!(part, Part::Mapped(q) if q.is_executable())
            })
        })
    }

    /// Where an admitted access of `[address, address + len)` should go. Lock-free, and one load
    /// when nothing traps (SUBPAGE-ORDER 2).
    #[must_use]
    pub fn access_ptr(&self, address: GuestAddr, len: usize) -> AccessPtr {
        let direct = AccessPtr::Direct(self.host_addr(address) as *mut u8);
        let Some(sub) = &self.sub else { return direct };
        if sub.count.load(std::sync::atomic::Ordering::Acquire) == 0 || len == 0 {
            return direct;
        }
        let page = self.page;
        let (first, last) = (address & !(page - 1), (address + len - 1) & !(page - 1));
        let (mut any, mut all) = (false, true);
        let mut p = first;
        while p <= last {
            let t = self.is_trapping(p);
            any |= t;
            all &= t;
            p += page;
        }
        match (any, all) {
            (false, _) => direct,
            (true, true) => {
                // The alias reservation exists once anything has trapped, and never goes while the
                // space lives. Its base is read without the overlay's lock: it is set once.
                let base = sub.alias_base.load(std::sync::atomic::Ordering::Acquire);
                AccessPtr::Alias((base + (address - self.base)) as *mut u8)
            }
            (true, false) => AccessPtr::Straddle,
        }
    }

    /// Call `f(guest address, pointer, len)` for each piece of `[address, address + len)` whose
    /// host pages are all trapping or all not: one call when nothing traps.
    pub fn for_each_access_chunk(&self, address: GuestAddr, len: usize, mut f: impl FnMut(GuestAddr, *mut u8, usize)) {
        if len == 0 {
            return;
        }
        let page = self.page;
        let end = address + len;
        let mut at = address;
        while at < end {
            let trapping = self.is_trapping(at);
            let mut next = ((at & !(page - 1)) + page).min(end);
            while next < end && self.is_trapping(next) == trapping {
                next = (next + page).min(end);
            }
            match self.access_ptr(at, next - at) {
                AccessPtr::Direct(p) | AccessPtr::Alias(p) => f(at, p, next - at),
                AccessPtr::Straddle => unreachable!("a chunk is uniform"),
            }
            at = next;
        }
    }

    /// Mark `[address, address + len)` so its unmapped parts must fault (`Part::StrictHole`), or
    /// no longer. Parts already unmapped are left as they are until mapped again.
    pub fn set_strict_gaps(&self, address: GuestAddr, len: usize, strict: bool) {
        let Some(sub) = &self.sub else { return };
        let _inner = self.write();
        let mut st = sub.state.lock();
        let end = address.saturating_add(len);
        st.strict.retain(|&(s, e)| !(s == address && e == end));
        if strict {
            st.strict.push((address, end));
        }
    }

    /// Count one access the slow path served through an alias. Never blocks.
    pub fn note_split_served(&self, address: GuestAddr) {
        let Some(sub) = &self.sub else { return };
        sub.served_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(mut st) = sub.state.try_lock() {
            *st.served.entry(address & !(self.page - 1)).or_default() += 1;
        }
    }

    /// The overlay's numbers.
    #[must_use]
    pub fn split_stats(&self) -> SplitStats {
        let Some(sub) = &self.sub else { return SplitStats::default() };
        let served_total = sub.served_total.load(std::sync::atomic::Ordering::Relaxed);
        let trapping = sub.count.load(std::sync::atomic::Ordering::Relaxed);
        let Some(st) = sub.state.try_lock() else { return SplitStats { trapping, served_total, ..SplitStats::default() } };
        let mut top: Vec<(GuestAddr, u64)> = st.served.iter().map(|(p, n)| (*p, *n)).collect();
        top.sort_by(|a, b| b.1.cmp(&a.1));
        top.truncate(8);
        SplitStats { tracked: st.split.len(), trapping, served_total, top }
    }
}

/// Expand tracked pages into runs of parts and merge what then continues. Pure.
fn expand(regions: Vec<RegionInfo>, split: &std::collections::BTreeMap<usize, Split>, page: usize, include_free: bool) -> Vec<RegionInfo> {
    let mut out: Vec<RegionInfo> = Vec::new();
    let push = |info: RegionInfo, out: &mut Vec<RegionInfo>| {
        if info.len == 0 || (info.is_free() && !include_free) {
            return;
        }
        match out.last_mut() {
            Some(previous) if previous.can_absorb(&info) => previous.absorb(&info),
            _ => out.push(info),
        }
    };
    for r in regions {
        let mut at = r.start;
        for (host_page, s) in split.range(r.start & !(page - 1)..r.end()) {
            let host_page = *host_page;
            if host_page >= r.end() || host_page + page <= r.start {
                continue;
            }
            if host_page > at {
                push(clip(&r, at, host_page), &mut out);
            }
            let mut i = 0;
            while i < s.parts.len() {
                let (first, count, part) = s.run_at(i);
                let (ps, pe) = (host_page + first * GUEST_PAGE, host_page + (first + count) * GUEST_PAGE);
                let mut piece = clip(&r, ps.max(r.start), pe.min(r.end()));
                match part {
                    Part::Mapped(protection) => piece.protection = protection,
                    Part::Hole | Part::StrictHole => {
                        piece = RegionInfo { kind: RegionKind::Free, protection: Protection::None, committed: 0, mapping: None, ..piece };
                    }
                }
                push(piece, &mut out);
                i = first + count;
            }
            at = host_page + page;
        }
        if at < r.end() {
            push(clip(&r, at, r.end()), &mut out);
        }
    }
    out
}

/// `r` cut to `[start, end)`.
fn clip(r: &RegionInfo, start: GuestAddr, end: GuestAddr) -> RegionInfo {
    let len = end.saturating_sub(start);
    RegionInfo { start, len, committed: if r.is_committed() { len } else { r.committed.min(len) }, ..r.clone() }
}
