//! **4 KiB guest pages on a larger host page** (spec `2026-09-29-4k-guest-pages`, D42).
//!
//! An arm64 Android guest built for 4 KiB pages maps and protects memory 4 KiB at a time; Apple
//! silicon's host page is 16 KiB. A space that asks for [`GUEST_PAGE`]s keeps, for each host page
//! the guest has cut into differently-treated 4 KiB [`Part`]s, what each part is ([`Split`]). The
//! host page gets the *least* any counted part allows ([`Split::host_protection`]), so every access
//! some part forbids faults; an access the guest's view allows but the host page refuses
//! ([`Split::traps`]) is served through a read-write alias of the page (`vm::alias`) by the slow
//! path. Execute is never asked of the host: guest code is fetched through the CPU backend's
//! callback, which asks the guest view.
//!
//! The invariants a space keeps for a tracked page: it is committed private anonymous memory; it
//! has a live alias while it [`traps`](Split::traps) (and possibly after -- an alias goes only when
//! its page is unmapped or replaced, SUBPAGE-ORDER); it leaves the overlay when it is
//! [`uniform`](Split::uniform) again (the host page is then exactly that) or [`empty`](Split::empty)
//! (the host page is unmapped); and every change to it bumps the space's generation.
//!
//! This module is the pure part: what a page's parts mean. `space.rs` applies it to the host.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

use omni_platform::vm::{Protection, Reservation};
use parking_lot::Mutex;

/// The guest's page: the smallest page an arm64 Linux guest is built for, and the one a space
/// that asks ([`crate::GuestSpaceConfig::guest_page`]) maps, protects and unmaps at, whatever the
/// host's page is.
pub const GUEST_PAGE: usize = 4096;

/// What one 4 KiB part of a tracked host page is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// Mapped, with the protection the guest asked for.
    Mapped(Protection),
    /// Not mapped. An access here may succeed (lenient, the default): it does not count toward
    /// the host page's protection.
    Hole,
    /// Not mapped, and an access here must fault: counts as [`Protection::None`] on the host
    /// (`GuestSpace::set_strict_gaps`).
    StrictHole,
}

/// One host page's parts, in address order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Split {
    pub(crate) parts: Vec<Part>,
}

/// Readable and writable bits of a protection; execute never matters to the host.
const fn bits(p: Protection) -> (bool, bool) {
    (p.is_readable(), p.is_writable())
}

impl Split {
    /// `parts_per_page` parts, each `fill`.
    pub(crate) fn new(parts_per_page: usize, fill: Part) -> Self {
        Self { parts: vec![fill; parts_per_page] }
    }

    /// `Some(p)` when every part is `Mapped(p)`: the page is uniform and leaves the overlay.
    pub(crate) fn uniform(&self) -> Option<Protection> {
        match self.parts.first()? {
            Part::Mapped(p) if self.parts.iter().all(|q| *q == Part::Mapped(*p)) => Some(*p),
            _ => None,
        }
    }

    /// Every part is a hole of either kind: the host page can be unmapped.
    pub(crate) fn empty(&self) -> bool {
        self.parts.iter().all(|p| matches!(p, Part::Hole | Part::StrictHole))
    }

    /// The host protection: readable if every counted part is, writable if every counted part is
    /// too. `Mapped` parts and `StrictHole`s (as nothing) count; `Hole`s do not. With nothing
    /// counted, `None`.
    pub(crate) fn host_protection(&self) -> Protection {
        let mut counted = false;
        let (mut read, mut write) = (true, true);
        for part in &self.parts {
            let (r, w) = match *part {
                Part::Mapped(p) => bits(p),
                Part::StrictHole => (false, false),
                Part::Hole => continue,
            };
            counted = true;
            read &= r;
            write &= w;
        }
        match (counted, read, write) {
            (true, true, true) => Protection::ReadWrite,
            (true, true, false) => Protection::Read,
            _ => Protection::None,
        }
    }

    /// Whether some `Mapped` part allows an access the host protection refuses: the page traps,
    /// and a served access needs its alias.
    pub(crate) fn traps(&self) -> bool {
        let host = bits(self.host_protection());
        self.parts.iter().any(|part| match *part {
            Part::Mapped(p) => {
                let (r, w) = bits(p);
                (r && !host.0) || (w && !host.1)
            }
            Part::Hole | Part::StrictHole => false,
        })
    }

    /// The run of equal parts containing part `i`: (first index, count, part).
    pub(crate) fn run_at(&self, i: usize) -> (usize, usize, Part) {
        let part = self.parts[i];
        let first = self.parts[..i].iter().rposition(|p| *p != part).map_or(0, |j| j + 1);
        let last = self.parts[i..].iter().position(|p| *p != part).map_or(self.parts.len(), |j| i + j);
        (first, last - first, part)
    }

    /// Set parts `[from, to)` to `part`.
    pub(crate) fn set(&mut self, from: usize, to: usize, part: Part) {
        self.parts[from..to].fill(part);
    }
}

/// A space's overlay, present only when it is active (the space asked for [`GUEST_PAGE`]s, the
/// host page is larger, and the host can alias).
///
/// The lock-free part is what the fault path reads (SUBPAGE-ORDER 2): `count` -- the number of
/// trapping host pages, zero almost always, which answers "no" with one load -- and one bit per
/// host page of the space. Everything else is in `state`, taken only while the space's own lock is
/// held (SUBPAGE-ORDER 1).
pub(crate) struct SubPagesHandle {
    /// Trapping host pages: the number of set bits.
    pub(crate) count: AtomicUsize,
    /// One bit per host page of the space, allocated at the first trapping page.
    pub(crate) bits: OnceLock<Box<[AtomicU64]>>,
    pub(crate) state: Mutex<SubPages>,
    /// Accesses the slow path served through an alias, for the report.
    pub(crate) served_total: AtomicU64,
}

/// The overlay's state: the tracked host pages, the alias reservation and what it holds.
pub(crate) struct SubPages {
    pub(crate) parts_per_page: usize,
    /// Host pages the guest has cut into differently-treated parts, by host page address.
    pub(crate) split: BTreeMap<usize, Split>,
    /// A reservation the size of the space, made at the first alias: host page `P`'s alias is at
    /// `alias_base + (P - space base)`.
    pub(crate) alias: Option<Reservation>,
    /// Host pages with a live alias -- trapping now, or once (an alias goes only with its page).
    pub(crate) aliased: BTreeSet<usize>,
    /// Guest ranges whose unmapped parts must fault (`[start, end)`).
    pub(crate) strict: Vec<(usize, usize)>,
    /// Accesses served, by host page.
    pub(crate) served: HashMap<usize, u64>,
}

impl SubPagesHandle {
    pub(crate) fn new(parts_per_page: usize) -> Self {
        Self {
            count: AtomicUsize::new(0),
            bits: OnceLock::new(),
            state: Mutex::new(SubPages {
                parts_per_page,
                split: BTreeMap::new(),
                alias: None,
                aliased: BTreeSet::new(),
                strict: Vec::new(),
                served: HashMap::new(),
            }),
            served_total: AtomicU64::new(0),
        }
    }

    /// Whether host page number `index` of the space traps. Lock-free (SUBPAGE-ORDER 2, 3).
    #[inline]
    pub(crate) fn is_trapping(&self, index: usize) -> bool {
        if self.count.load(Ordering::Acquire) == 0 {
            return false;
        }
        self.bits.get().and_then(|b| b.get(index / 64)).is_some_and(|w| w.load(Ordering::Acquire) & (1 << (index % 64)) != 0)
    }

    /// Set or clear host page `index`'s bit, keeping `count` the number of set bits. `pages` is
    /// the space's length in host pages (for the first allocation). Called with the space's lock
    /// held.
    pub(crate) fn set_trapping(&self, index: usize, pages: usize, on: bool) {
        let bits = self.bits.get_or_init(|| (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect());
        let word = &bits[index / 64];
        let mask = 1u64 << (index % 64);
        let was = if on { word.fetch_or(mask, Ordering::Release) } else { word.fetch_and(!mask, Ordering::Release) };
        match (was & mask != 0, on) {
            (false, true) => {
                self.count.fetch_add(1, Ordering::Release);
            }
            (true, false) => {
                self.count.fetch_sub(1, Ordering::Release);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Protection::{Read, ReadExecute, ReadWrite, ReadWriteExecute};

    fn split(parts: &[Part]) -> Split {
        Split { parts: parts.to_vec() }
    }

    #[test]
    fn host_protection_is_the_least_any_counted_part_allows() {
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::Mapped(Read)]).host_protection(), Read);
        assert_eq!(split(&[Part::Mapped(ReadExecute), Part::Mapped(ReadWrite)]).host_protection(), Read);
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::Mapped(Protection::None)]).host_protection(), Protection::None);
        assert_eq!(split(&[Part::Mapped(ReadWriteExecute), Part::Mapped(ReadWrite)]).host_protection(), ReadWrite);
    }

    #[test]
    fn a_lenient_hole_does_not_count_and_a_strict_one_does() {
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::Hole]).host_protection(), ReadWrite);
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::StrictHole]).host_protection(), Protection::None);
        assert_eq!(split(&[Part::Hole, Part::Hole]).host_protection(), Protection::None);
    }

    #[test]
    fn a_page_traps_only_when_a_mapped_part_allows_more_than_the_host() {
        assert!(!split(&[Part::Mapped(ReadWrite), Part::Hole]).traps());
        assert!(split(&[Part::Mapped(ReadWrite), Part::Mapped(Read)]).traps());
        assert!(!split(&[Part::Mapped(Protection::None), Part::StrictHole]).traps());
        assert!(split(&[Part::Mapped(Read), Part::StrictHole]).traps());
        // Execute alone never traps: guest code is fetched in software.
        assert!(!split(&[Part::Mapped(Read), Part::Mapped(ReadExecute)]).traps());
    }

    #[test]
    fn uniform_and_empty() {
        assert_eq!(split(&[Part::Mapped(Read), Part::Mapped(Read)]).uniform(), Some(Read));
        assert_eq!(split(&[Part::Mapped(Read), Part::Hole]).uniform(), None);
        assert_eq!(split(&[Part::Mapped(Read), Part::Mapped(ReadExecute)]).uniform(), None);
        assert!(split(&[Part::Hole, Part::StrictHole]).empty());
        assert!(!split(&[Part::Hole, Part::Mapped(Protection::None)]).empty());
    }

    #[test]
    fn runs_and_set() {
        let mut s = Split::new(4, Part::Hole);
        s.set(1, 3, Part::Mapped(ReadWrite));
        assert_eq!(s.run_at(0), (0, 1, Part::Hole));
        assert_eq!(s.run_at(2), (1, 2, Part::Mapped(ReadWrite)));
        assert_eq!(s.run_at(1), (1, 2, Part::Mapped(ReadWrite)));
        assert_eq!(s.run_at(3), (3, 1, Part::Hole));
    }
}
