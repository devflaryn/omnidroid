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

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::{GuestAddr, GuestSpace, Inner, OsState, Placement, SpaceUsage};
use crate::backing::Backing;
use crate::entry::{Entry, Owner};
use crate::error::{platform, MemError, MemResult};
use crate::region::{RegionInfo, RegionKind};
use crate::subpage::{Part, Split, SubPages, SubPagesHandle, GUEST_PAGE};
use crate::{CommitPolicy, MappingId};
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

/// How [`GuestSpace::map_file_exact`] puts its view under an edge host page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    /// The page is free: a new view, its other parts holes.
    Free,
    /// A never-touched anonymous page whose other parts are `PROT_NONE` or holes (the dynamic
    /// linker's reservation around a library): a new view, those parts foreign (`Split::foreign`).
    Reserved,
    /// Already a view of the same file at the same offset that was never writable (the library's
    /// neighbouring segment, from the same file or shifted file): no new view.
    Same,
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
    /// Each of `top`'s parts, 4 KiB at a time: `rw`, `r`, `-` (PROT_NONE), `.` (a hole).
    pub top_parts: Vec<String>,
}

impl GuestSpace {
    fn sub_handle(&self) -> &SubPagesHandle {
        self.sub.as_ref().expect("the overlay is active")
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
        if matches!(op, SubOp::Map(..) | SubOp::Unmap) && !st.sealed.is_empty() {
            let end = address.saturating_add(len);
            st.sealed.retain(|&(s, e)| !(s < end && address < e));
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
                // A page leaving the overlay with foreign parts: their bytes become their own (zero)
                // first, or the whole page would be the view's.
                for &host_page in &tracked {
                    let foreign = st.split.get(&host_page).map_or(0, |s| s.foreign);
                    if foreign != 0 {
                        self.privatise(inner, st, operation, host_page, foreign)?;
                    }
                }
                // Host first, then the bits (SUBPAGE-ORDER 3).
                inner.protect_range(operation, address, len, protection)?;
            }
            SubOp::Unmap => {
                inner.unmap_range(operation, address, len)?;
            }
        }
        for host_page in tracked {
            st.split.remove(&host_page);
            self.sub_handle().set_trapping(host_page, false);
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
        let range = Split::mask(i0, i1);
        let (fill, zero, eager) = match op {
            SubOp::Map(protection, commit) => (Part::Mapped(protection), range, commit == CommitPolicy::Eager),
            // A foreign part made accessible reads zero, as the reservation it is (`Split::foreign`).
            SubOp::Protect(protection) => (Part::Mapped(protection), if protection == Protection::None { 0 } else { split.foreign & range }, false),
            SubOp::Unmap => (if Self::is_strict(st, at, len) { Part::StrictHole } else { Part::Hole }, 0, false),
        };
        let mapped_here = !st.split.contains_key(&host_page) && split.empty();
        split.set(i0, i1, fill);
        if !matches!(op, SubOp::Protect(Protection::None)) {
            split.foreign &= !range;
        }
        let settled = self.settle(inner, st, operation, host_page, split, zero, eager);
        if settled.is_err() && mapped_here {
            // The host page this call mapped for a 4 KiB map goes again (checkpoint A, 4).
            let _ = inner.unmap_range(operation, host_page, page);
        }
        settled
    }

    fn is_strict(st: &SubPages, at: GuestAddr, len: usize) -> bool {
        st.strict.iter().any(|&(s, e)| s < at + len && at < e)
    }

    /// Whether the 4 KiB part at `at` lies in a sealed RELRO range.
    fn is_sealed(st: &SubPages, at: GuestAddr) -> bool {
        st.sealed.iter().any(|&(s, e)| s <= at && at + GUEST_PAGE <= e)
    }

    /// `[address, address + len)` is unmapped or mapped over: the sealed ranges it touches go.
    pub(super) fn forget_sealed(&self, address: GuestAddr, len: usize) {
        if let Some(sub) = &self.sub {
            let mut st = sub.state.lock();
            if !st.sealed.is_empty() {
                let end = address.saturating_add(len);
                st.sealed.retain(|&(s, e)| !(s < end && address < e));
            }
        }
    }

    /// **`[address, address + len)` is a library's RELRO, sealed read-only by the dynamic linker**:
    /// its read-only parts beside a writable part of their host page need not trap
    /// (`Split::host_protection_lenient`). Said before the `protect` that seals it, which then
    /// takes it into account; forgotten where the range is unmapped or mapped over.
    pub fn set_sealed_read_only(&self, address: GuestAddr, len: usize) {
        let Some(sub) = &self.sub else { return };
        let _inner = self.write();
        let mut st = sub.state.lock();
        let end = address.saturating_add(len);
        if !st.sealed.iter().any(|&(s, e)| s == address && e == end) {
            st.sealed.push((address, end));
        }
    }

    /// The host protection a page with parts `split` gets (lenient where it may be), and the strict
    /// one it falls back to: `(host, strict_host)`.
    fn planned_host(&self, inner: &Inner, st: &SubPages, host_page: GuestAddr, split: &Split) -> (Protection, Protection) {
        // PROT_NONE parts count toward the host page only where the guest asked for strictness.
        let strict = crate::subpage::strict_prot_none_everywhere() || Self::is_strict(st, host_page, self.page);
        // Read-only parts beside a writable one leniently (`Split::host_protection_lenient`) --
        // not on a view of a shared backing, whose writes would reach every other view of it.
        let shared = inner
            .map
            .entry_start(host_page)
            .and_then(|s| inner.map.get(s))
            .is_some_and(|e| matches!(e.os, OsState::View { .. }) && e.owner.as_ref().and_then(|o| o.backing.as_ref()).is_some_and(|b| b.is_shared()));
        let lenient = crate::subpage::lenient_read_only() && !shared;
        let strict_host = split.host_protection(strict);
        let sealed = |i: usize| Self::is_sealed(st, host_page + i * GUEST_PAGE);
        (if lenient { split.host_protection_lenient(strict, sealed) } else { strict_host }, strict_host)
    }

    /// Put a changed host page's parts into effect, in SUBPAGE-ORDER. `zero`: parts (bits) newly
    /// mapped, which must read zero.
    fn settle(
        &self,
        inner: &mut Inner,
        st: &mut SubPages,
        operation: &'static str,
        host_page: GuestAddr,
        mut split: Split,
        zero: u64,
        eager: bool,
    ) -> MemResult<()> {
        let sub = self.sub_handle();
        let page = self.page;
        let committed = |inner: &Inner| {
            inner.map.entry_start(host_page).and_then(|s| inner.map.get(s)).is_some_and(|e| matches!(e.os, OsState::Private { .. } | OsState::View { .. }))
        };
        if split.empty() {
            sub.set_trapping(host_page, false);
            st.split.remove(&host_page);
            inner.unmap_range(operation, host_page, page)?;
            self.unalias_range(st, host_page, page);
            return Ok(());
        }
        let (mut host, strict_host) = self.planned_host(inner, st, host_page, &split);
        let needs_view = split.traps_under(host) || eager || (zero != 0 && committed(inner));
        if needs_view {
            self.commit_page(inner, operation, host_page)?;
            self.privatise(inner, st, operation, host_page, split.foreign)?;
            split.foreign = 0;
            self.ensure_alias(st, host_page)?;
        }
        if zero != 0 && committed(inner) {
            let alias = self.alias_of(st, host_page);
            for i in (0..split.parts.len()).filter(|i| zero >> i & 1 != 0) {
                // SAFETY: the alias is a live read-write view of this committed host page (just
                // made), and the parts being zeroed are ones the guest has just mapped afresh.
                unsafe { std::ptr::write_bytes((alias + i * GUEST_PAGE) as *mut u8, 0, GUEST_PAGE) };
            }
        }
        if let Some(protection) = split.uniform().filter(|_| split.foreign == 0) {
            // Leaves the overlay: host first, then the bit (the alias stays, SUBPAGE-ORDER 5).
            inner.protect_range(operation, host_page, page, protection)?;
            sub.set_trapping(host_page, false);
            st.split.remove(&host_page);
            return Ok(());
        }
        if host != strict_host && inner.protect_range(operation, host_page, page, host).is_err() {
            // The host refused the lenient protection: the page is held as strictly as before.
            host = strict_host;
            if split.traps_under(host) {
                self.commit_page(inner, operation, host_page)?;
                self.privatise(inner, st, operation, host_page, split.foreign)?;
                split.foreign = 0;
                self.ensure_alias(st, host_page)?;
            }
        }
        if split.traps_under(host) {
            let was = sub.is_trapping(host_page);
            sub.set_trapping(host_page, true);
            if let Err(e) = inner.protect_range(operation, host_page, page, host) {
                // Nothing published: the page keeps its old parts and its old bit.
                sub.set_trapping(host_page, was);
                return Err(e);
            }
        } else {
            inner.protect_range(operation, host_page, page, host)?;
            sub.set_trapping(host_page, false);
        }
        st.split.insert(host_page, split);
        Ok(())
    }

    /// [`GuestSpace::map_file_exact`] with the overlay active: `[address, address + len)` is
    /// 4 KiB-exact and `file_offset` agrees with `address` modulo the host page.
    pub(super) fn map_file_edges(&self, backing: &Arc<Backing>, file_offset: u64, address: GuestAddr, len: usize, protection: Protection) -> MemResult<(GuestAddr, usize)> {
        const OP: &str = "map_file_exact";
        let page = self.page;
        let mut inner = self.write();
        let sub = self.sub_handle();
        let mut st = sub.state.lock();
        if !self.guest_view_is(&inner, &st, address, len, false) {
            return Err(MemError::AddressTaken {
                operation: OP,
                requested: address,
                requested_end: address + len,
                conflict_start: address,
                conflict_end: address + len,
                conflict: "a mapped 4 KiB page",
            });
        }
        let end = address + len;
        let (first, last) = (address & !(page - 1), (end - 1) & !(page - 1));
        let offset_at = |at: GuestAddr| file_offset + at as u64 - address as u64;
        // Both ends inside one host page: one edge.
        let single = first == last && address % page != 0 && end % page != 0;
        let (head, tail) = {
            let edge = |at: GuestAddr, from: GuestAddr, to: GuestAddr| {
                self.edge(&inner, &st, at, Split::mask((from - at) / GUEST_PAGE, (to - at) / GUEST_PAGE), backing, offset_at(at), protection)
            };
            ((address % page != 0).then(|| edge(first, address, end.min(first + page))), (end % page != 0 && !single).then(|| edge(last, last.max(address), end)))
        };
        let taken = |e: &Option<Option<(Edge, Split)>>| !matches!(e, Some(None));
        let fresh = |e: &Option<Option<(Edge, Split)>>| matches!(e, Some(Some((Edge::Free | Edge::Reserved, _))));
        // The part made a view, and the host pages that need a new one.
        let from = if taken(&head) { address } else { first + page };
        let to = if single || taken(&tail) { end } else { last };
        if from >= to {
            return Ok((address, 0));
        }
        let lo = match &head {
            None => address,
            Some(_) if fresh(&head) => first,
            Some(_) => first + page,
        };
        let hi = match &tail {
            _ if single => if fresh(&head) { first + page } else { lo },
            None => end,
            Some(_) if fresh(&tail) => last + page,
            Some(_) => last,
        };
        let (head_fresh, tail_fresh) = (fresh(&head), !single && fresh(&tail));
        let edges: Vec<(GuestAddr, Edge, Split)> = [(first, head), (last, tail)].into_iter().filter_map(|(at, e)| e.flatten().map(|(k, s)| (at, k, s))).collect();
        if lo < hi {
            // A reservation's host page leaves it first -- never committed, nothing to give back --
            // so the new view's range is free, and goes back to it if the view cannot be made.
            let reserved: Vec<(GuestAddr, Option<Owner>)> = edges
                .iter()
                .filter(|(_, k, _)| *k == Edge::Reserved)
                .map(|(at, _, _)| (*at, inner.map.entry_start(*at).and_then(|s| inner.map.get(s)).and_then(|e| e.owner.clone())))
                .collect();
            for &(at, _) in &reserved {
                inner.unmap_range(OP, at, page)?;
            }
            // One mapping, but each new edge page an OS view of its own: made private later -- a
            // `.bss` mapped over its reservation part -- it unmaps that view alone, where one view of
            // the whole range would be unmapped, mapped again and scanned for copy-on-write pages
            // (`Inner::unmap_view`) at each library's load.
            let whole = (address.next_multiple_of(page), end & !(page - 1));
            let mut pieces: Vec<(GuestAddr, usize)> = Vec::new();
            if head_fresh {
                pieces.push((first, page));
            }
            if whole.0 < whole.1 {
                pieces.push((whole.0, whole.1 - whole.0));
            }
            if tail_fresh {
                pieces.push((last, page));
            }
            let owner = Owner {
                id: MappingId(super::NEXT_MAPPING.fetch_add(1, Ordering::Relaxed)),
                mapping_start: lo,
                mapping_len: hi - lo,
                protection,
                granule: inner.granule,
                commit: CommitPolicy::Eager,
                backing: Some(Arc::clone(backing)),
                file_offset: offset_at(lo),
                label: crate::label::current(),
            };
            let ever_writable = protection.is_writable() && !backing.is_shared();
            let mut made: Vec<(GuestAddr, usize)> = Vec::new();
            for (at, n) in pieces {
                match inner.make_exact_placeholder(OP, at, n, true).and_then(|()| inner.map_view(OP, backing, offset_at(at), at, n, protection)) {
                    Ok(view) => {
                        inner.map.replace(at, n, Entry { len: n, os: OsState::View { view }, owner: Some(owner.clone()), ever_writable });
                        made.push((at, n));
                    }
                    Err(e) => {
                        for (at, n) in made {
                            let _ = inner.unmap_range(OP, at, n);
                        }
                        for (at, owner) in reserved {
                            if inner.make_exact_placeholder(OP, at, page, true).is_ok() {
                                inner.map.replace(at, page, Entry { len: page, os: OsState::Placeholder, owner, ever_writable: false });
                            }
                        }
                        inner.validate();
                        return Err(e);
                    }
                }
            }
            debug_assert_eq!(made.first().map(|m| m.0), Some(lo), "the pieces start the range");
            debug_assert_eq!(made.last().map(|m| m.0 + m.1), Some(hi), "and end it");
            // Replaced pages' aliases go (SUBPAGE-ORDER 5).
            self.unalias_range(&mut st, lo, hi - lo);
        }
        st.sealed.retain(|&(s, e)| !(s < end && address < e));
        // Each edge's parts first, so that undoing a failed settle (below) sees what was mapped.
        for (at, _, split) in &edges {
            st.split.insert(*at, split.clone());
        }
        for (at, _, split) in edges {
            if let Err(e) = self.settle(&mut inner, &mut st, OP, at, split, 0, false) {
                // Only what this call mapped goes: its parts, through the overlay.
                drop(st);
                let _ = self.sub_apply(&mut inner, OP, from, to - from, SubOp::Unmap);
                inner.validate();
                return Err(e);
            }
        }
        drop(st);
        inner.validate();
        Ok((from, to - from))
    }

    /// Whether [`map_file_edges`](Self::map_file_edges)'s view can sit under edge host page
    /// `host_page`, whose parts `ours` (bits) it maps from `offset` (the page's file offset): how,
    /// and the page's parts with it in place. `None` leaves the page out, for the caller to copy
    /// its part as before.
    #[allow(clippy::too_many_arguments)]
    fn edge(&self, inner: &Inner, st: &SubPages, host_page: GuestAddr, ours: u64, backing: &Backing, offset: u64, protection: Protection) -> Option<(Edge, Split)> {
        // A shared view's page is aliased as it is (`privatise`), and its holes would be the file.
        if backing.is_shared() {
            return None;
        }
        let entry = inner.map.get(inner.map.entry_start(host_page)?)?;
        let (kind, mut split) = if entry.is_free() {
            (Edge::Free, Split::new(st.parts_per_page, Part::Hole))
        } else {
            let split = st.split.get(&host_page)?.clone();
            let owner = entry.owner.as_ref()?;
            let reserve = split.parts.iter().enumerate().all(|(i, p)| ours >> i & 1 != 0 || matches!(p, Part::Hole | Part::StrictHole | Part::Mapped(Protection::None)));
            match (entry.os, owner.backing.as_ref()) {
                (OsState::Placeholder, None) if reserve => (Edge::Reserved, split),
                // Never writable: no copy-on-write page holds bytes the file does not.
                (OsState::View { .. }, Some(b)) if b.id() == backing.id() && !entry.ever_writable && owner.offset_at(host_page) == offset => (Edge::Same, split),
                _ => return None,
            }
        };
        for i in 0..split.parts.len() {
            if ours >> i & 1 != 0 {
                split.parts[i] = Part::Mapped(protection);
            } else if kind == Edge::Reserved && split.parts[i] == Part::Mapped(Protection::None) {
                split.foreign |= 1 << i;
            }
        }
        // A page that would trap is privatised by `settle`: a copy, which the caller makes anyway.
        let (host, _) = self.planned_host(inner, st, host_page, &split);
        (!split.traps_under(host)).then_some((kind, split))
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
    /// aliased without depending on how the host shares a copy-on-write view. Its `foreign` parts
    /// (bits, `Split::foreign`) get zeros instead: the reservation's own bytes. The caller clears
    /// them from the page's split.
    fn privatise(&self, inner: &mut Inner, st: &mut SubPages, operation: &'static str, host_page: GuestAddr, foreign: u64) -> MemResult<()> {
        let page = self.page;
        let Some(start) = inner.map.entry_start(host_page) else { return Ok(()) };
        let entry = inner.map.get(start).expect("entry vanished");
        if !matches!(entry.os, OsState::View { .. }) {
            return Ok(());
        }
        let owner = entry.owner.clone();
        // A view of a shared backing (memfd, ashmem, a MAP_SHARED file) is the memory every other
        // view of it shares: aliasing the view itself keeps that, and there is no copy-on-write
        // to depend on (checkpoint A, 3).
        if owner.as_ref().and_then(|o| o.backing.as_ref()).is_some_and(|b| b.is_shared()) {
            return Ok(());
        }
        let recorded = owner.as_ref().map_or(Protection::Read, |o| o.protection);
        // The private copy's commit, checked before the view is touched: refused, nothing changes
        // (checkpoint A, 2).
        inner.check_commit_allowed(operation, host_page, page)?;
        if !recorded.is_readable() {
            inner.protect_range(operation, host_page, page, Protection::Read)?;
        }
        let mut bytes = vec![0u8; page];
        // SAFETY: the page is a live view this space owns, readable now; nothing writes it while
        // `inner` is held but guest code, whose writes the copy may or may not see -- as a
        // concurrent write during the guest's own mprotect may or may not land.
        unsafe { std::ptr::copy_nonoverlapping(inner.host(host_page) as *const u8, bytes.as_mut_ptr(), page) };
        for (i, part) in bytes.chunks_mut(GUEST_PAGE).enumerate() {
            if foreign >> i & 1 != 0 {
                part.fill(0);
            }
        }
        inner.unmap_range(operation, host_page, page)?;
        self.unalias_range(st, host_page, page);
        self.map_anonymous_locked(inner, operation, Placement::Fixed(host_page), page, Protection::ReadWrite, CommitPolicy::Eager)?;
        // SAFETY: just mapped, committed and read-write.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), inner.host(host_page) as *mut u8, page) };
        // The copy is still the file's page to the guest: the view's own owner -- mapping, file and
        // offset -- on committed private memory, so `/proc/<pid>/maps` names the library there
        // (checkpoint A, 5).
        if let Some(mut owner) = owner {
            owner.protection = Protection::ReadWrite;
            inner.map.get_mut(host_page).expect("just mapped").owner = Some(owner);
        }
        inner.protect_range(operation, host_page, page, recorded)?;
        Ok(())
    }

    fn alias_of(&self, st: &SubPages, host_page: GuestAddr) -> usize {
        let _ = st; // held: the alias cannot go meanwhile
        self.sub_handle().alias_of(host_page).expect("an aliased page")
    }

    /// Make host page `host_page`'s alias if it has none, placed where the host has room (one page
    /// of address space; a space may be terabytes). **A live alias is never re-made**: a served
    /// access may be using it (checkpoint A, 1). So every path that decommits or replaces a page
    /// unmaps its alias first (`unalias_range`, `forget_aliases`), and an alias in `aliased` is
    /// always the page's memory (SUBPAGE-ORDER 5).
    fn ensure_alias(&self, st: &mut SubPages, host_page: GuestAddr) -> MemResult<()> {
        if st.aliased.contains(&host_page) {
            return Ok(());
        }
        // SAFETY: the source is committed private memory of this space (`commit_page`,
        // `privatise`), or a live view of a shared backing.
        let at = unsafe { vm::alias(self.host_addr(host_page) as *mut u8, self.page) }.map_err(platform("alias", host_page, self.page))?;
        self.sub_handle().insert_alias(host_page, at as usize);
        st.aliased.insert(host_page);
        Ok(())
    }

    /// Unmap the aliases of host pages in `[address, address + len)`, after those pages were
    /// unmapped or replaced (SUBPAGE-ORDER 5).
    pub(super) fn unalias_range(&self, st: &mut SubPages, address: GuestAddr, len: usize) {
        let gone: Vec<usize> = st.aliased.range(address..address + len).copied().collect();
        for host_page in gone {
            st.aliased.remove(&host_page);
            let Some(at) = self.sub_handle().remove_alias(host_page) else { continue };
            // SAFETY: an alias this overlay made; its page is gone, so nothing may reach it.
            if let Err(e) = unsafe { vm::unalias(at as *mut u8, self.page) } {
                tracing::error!(%e, host_page = format_args!("{host_page:#x}"), "an alias could not be unmapped");
            }
        }
    }

    /// Every alias unmapped: the space is going.
    pub(super) fn unalias_all(&self) {
        if let Some(sub) = &self.sub {
            let mut st = sub.state.lock();
            self.unalias_range(&mut st, self.base, self.len);
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
            let start = host_page + first * GUEST_PAGE;
            if split.is_foreign(first) {
                // A reservation's part over the page's view: anonymous, holding nothing.
                return Some(RegionInfo { start, len: count * GUEST_PAGE, protection, committed: 0, kind: RegionKind::Anonymous, ..info });
            }
            return Some(RegionInfo {
                start,
                len: count * GUEST_PAGE,
                protection,
                committed: if info.committed > 0 { count * GUEST_PAGE } else { 0 },
                kind: rebased(&info, start),
                ..info
            });
        }
        let lo = st.split.range(..host_page).next_back().map_or(info.start, |(p, _)| (p + page).max(info.start));
        let hi = st.split.range(host_page..).next().map_or(info.end(), |(p, _)| (*p).min(info.end()));
        Some(RegionInfo { start: lo, len: hi - lo, committed: if info.is_committed() { hi - lo } else { 0 }, kind: rebased(&info, lo), ..info })
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

    /// [`GuestSpace::usage`]'s pass, `inner` held: every mapped entry (the host's ranges too, as
    /// `mapped_regions` lists them), less the holes of the tracked pages it covers, which are
    /// walked in step in address order. A page that runs past an entry's end is kept for the next.
    pub(super) fn guest_usage(&self, inner: &Inner) -> SpaceUsage {
        let st = self.sub.as_ref().map(|sub| sub.state.lock());
        let mut tracked = st.as_ref().filter(|st| !st.split.is_empty()).map(|st| st.split.iter().peekable());
        let page = self.page;
        let mut usage = SpaceUsage::default();
        for (start, entry) in inner.map.iter() {
            if entry.is_free() {
                continue;
            }
            let end = start + entry.len;
            let private = matches!(entry.os, OsState::Private { .. });
            usage.mapped += entry.len;
            if private {
                usage.committed += entry.len;
            }
            let Some(pages) = tracked.as_mut() else { continue };
            while pages.next_if(|(p, _)| **p + page <= start).is_some() {}
            for (p, split) in pages.clone().take_while(|(p, _)| **p < end) {
                for (i, part) in split.parts.iter().enumerate() {
                    if matches!(part, Part::Hole | Part::StrictHole) {
                        let (from, to) = ((p + i * GUEST_PAGE).max(start), (p + (i + 1) * GUEST_PAGE).min(end));
                        let hole = to.saturating_sub(from);
                        usage.mapped -= hole;
                        if private {
                            usage.committed -= hole;
                        }
                    }
                }
            }
        }
        usage
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
    /// (`discard`). Returns the bytes zeroed. A view of a shared backing is left alone, as
    /// `discard` leaves an untracked one: its page is the file's (`privatise` keeps it so), and
    /// zeros written through its alias were written into the file -- or, on a page wholly past the
    /// file's end, a host `SIGBUS` at the alias, outside the guest space, which killed the host
    /// process. Linux's `MADV_DONTNEED` leaves a shared mapping's file as it is.
    pub(super) fn zero_tracked(&self, inner: &mut Inner, operation: &'static str, at: GuestAddr, len: usize) -> MemResult<usize> {
        let sub = self.sub_handle();
        let mut st = sub.state.lock();
        let page = self.page;
        let mut zeroed = 0;
        let mut p = at & !(page - 1);
        while p < at + len {
            let committed = inner.map.entry_start(p).and_then(|s| inner.map.get(s)).is_some_and(|e| match e.os {
                OsState::Private { .. } => true,
                OsState::View { .. } => !e.owner.as_ref().and_then(|o| o.backing.as_ref()).is_some_and(|b| b.is_shared()),
                _ => false,
            });
            if committed {
                let foreign = st.split.get(&p).map_or(0, |s| s.foreign);
                self.privatise(inner, &mut st, operation, p, foreign)?;
                if let Some(s) = st.split.get_mut(&p) {
                    s.foreign = 0;
                }
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
            let foreign = st.split.get(&p).map_or(0, |s| s.foreign);
            self.privatise(inner, &mut st, operation, p, foreign)?;
            if let Some(s) = st.split.get_mut(&p) {
                s.foreign = 0;
            }
            self.ensure_alias(&mut st, p)?;
            p += page;
        }
        let mut done = 0;
        while done < bytes.len() {
            let g = at + done;
            let n = (page - g % page).min(bytes.len() - done);
            // SAFETY: every host page of the range is committed and has a live read-write alias.
            unsafe { std::ptr::copy_nonoverlapping(bytes[done..].as_ptr(), (self.alias_of(&st, g & !(page - 1)) + g % page) as *mut u8, n) };
            done += n;
        }
        Ok(())
    }

    /// `held_ranges` in the guest's view: a tracked page's holes and foreign parts are not held.
    /// `inner` is held.
    pub(super) fn guest_held(&self, held: Vec<(GuestAddr, usize)>) -> Vec<(GuestAddr, usize)> {
        let Some(sub) = &self.sub else { return held };
        let st = sub.state.lock();
        if st.split.is_empty() {
            return held;
        }
        let page = self.page;
        let mut out: Vec<(GuestAddr, usize)> = Vec::new();
        let mut push = |s: usize, e: usize| {
            if s >= e {
                return;
            }
            match out.last_mut() {
                Some(last) if last.0 + last.1 == s => last.1 += e - s,
                _ => out.push((s, e - s)),
            }
        };
        for (start, len) in held {
            let end = start + len;
            let mut at = start;
            while at < end {
                let host_page = at & !(page - 1);
                let next = (host_page + page).min(end);
                match st.split.get(&host_page) {
                    Some(s) => {
                        let mut g = at;
                        while g < next {
                            let gn = ((g & !(GUEST_PAGE - 1)) + GUEST_PAGE).min(next);
                            let i = (g - host_page) / GUEST_PAGE;
                            if matches!(s.parts[i], Part::Mapped(_)) && !s.is_foreign(i) {
                                push(g, gn);
                            }
                            g = gn;
                        }
                    }
                    None => push(at, next),
                }
                at = next;
            }
        }
        out
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

    /// Where an admitted access of `[address, address + len)` should go. One atomic load when
    /// nothing traps (SUBPAGE-ORDER 2); else the alias map's read lock.
    #[must_use]
    pub fn access_ptr(&self, address: GuestAddr, len: usize) -> AccessPtr {
        let direct = AccessPtr::Direct(self.host_addr(address) as *mut u8);
        let Some(sub) = &self.sub else { return direct };
        if sub.count.load(std::sync::atomic::Ordering::Acquire) == 0 || len == 0 {
            return direct;
        }
        let page = self.page;
        let (first, last) = (address & !(page - 1), (address + len - 1) & !(page - 1));
        if first == last {
            return match sub.trapping_alias(first) {
                Some(at) => AccessPtr::Alias((at + address % page) as *mut u8),
                None => direct,
            };
        }
        // Across host pages: one pointer only if none of them traps (aliases are not contiguous).
        let mut p = first;
        while p <= last {
            if sub.is_trapping(p) {
                return AccessPtr::Straddle;
            }
            p += page;
        }
        direct
    }

    /// Call `f(guest address, pointer, len)` for each piece of `[address, address + len)`: a run
    /// of ordinary host pages as one piece, a trapping host page's part through its alias. One call
    /// when nothing traps.
    pub fn for_each_access_chunk(&self, address: GuestAddr, len: usize, mut f: impl FnMut(GuestAddr, *mut u8, usize)) {
        if len == 0 {
            return;
        }
        let Some(sub) = self.sub.as_ref().filter(|s| s.count.load(std::sync::atomic::Ordering::Acquire) != 0) else {
            f(address, self.host_addr(address) as *mut u8, len);
            return;
        };
        let page = self.page;
        let end = address + len;
        let mut at = address;
        while at < end {
            let next = ((at & !(page - 1)) + page).min(end);
            // One look at each page, and the pointer from that look (checkpoint A, 8).
            match sub.trapping_alias(at & !(page - 1)) {
                Some(alias) => {
                    f(at, (alias + at % page) as *mut u8, next - at);
                    at = next;
                }
                None => {
                    let mut run_end = next;
                    while run_end < end && sub.trapping_alias(run_end).is_none() {
                        run_end = (run_end + page).min(end);
                    }
                    f(at, self.host_addr(at) as *mut u8, run_end - at);
                    at = run_end;
                }
            }
        }
    }

    /// Mark `[address, address + len)` strict, or no longer: its unmapped parts must fault
    /// (`Part::StrictHole`), and so must its PROT_NONE parts even beside accessible ones. Takes
    /// effect on the next change to each host page.
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
        let top_parts = top
            .iter()
            .map(|(page, _)| {
                st.split.get(page).map_or_else(String::new, |s| {
                    s.parts
                        .iter()
                        .map(|p| match p {
                            Part::Mapped(Protection::ReadWriteExecute) => "rwx",
                            Part::Mapped(Protection::ReadExecute) => "rx",
                            Part::Mapped(p) if p.is_writable() => "rw",
                            Part::Mapped(p) if p.is_readable() => "r",
                            Part::Mapped(_) => "-",
                            Part::Hole | Part::StrictHole => ".",
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            })
            .collect();
        SplitStats { tracked: st.split.len(), trapping, served_total, top, top_parts }
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
                    Part::Mapped(protection) if s.is_foreign(first) => {
                        piece = RegionInfo { kind: RegionKind::Anonymous, protection, committed: 0, ..piece };
                    }
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
    RegionInfo { start, len, committed: if r.is_committed() { len } else { r.committed.min(len) }, kind: rebased(r, start), ..r.clone() }
}

/// `r`'s kind for a piece of it starting at `start`: a file's offset moves with the start.
fn rebased(r: &RegionInfo, start: GuestAddr) -> RegionKind {
    match &r.kind {
        RegionKind::File { backing, name, file_offset, shared, guest_named } => RegionKind::File {
            backing: *backing,
            name: std::sync::Arc::clone(name),
            file_offset: file_offset + (start - r.start) as u64,
            shared: *shared,
            guest_named: *guest_named,
        },
        other => other.clone(),
    }
}
