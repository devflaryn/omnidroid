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
//! The invariants a space keeps for a tracked page: it is committed private anonymous memory, or a
//! file view while nothing in it traps (only a view holds [`foreign`](Split::foreign) parts); it
//! has a live alias while it [`traps`](Split::traps) (and possibly after -- an alias goes only when
//! its page is unmapped or replaced, SUBPAGE-ORDER); it leaves the overlay when it is
//! [`uniform`](Split::uniform) again with no foreign part (the host page is then exactly that) or
//! [`empty`](Split::empty) (the host page is unmapped); and every change to it bumps the space's
//! generation.
//!
//! This module is the pure part: what a page's parts mean. `space.rs` applies it to the host.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

use omni_platform::vm::Protection;
use parking_lot::{Mutex, RwLock};

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
    /// Parts (bit `i`, part `i`) whose bytes are not their own: a never-touched `PROT_NONE`
    /// reservation's parts on a host page that is another mapping's file view
    /// (`GuestSpace::map_file_exact`). Each is `Mapped(Protection::None)`; it reads zero once it is
    /// made accessible or the page stops being a view (`privatise`), as anonymous memory does.
    pub(crate) foreign: u64,
}

/// Readable and writable bits of a protection; execute never matters to the host.
const fn bits(p: Protection) -> (bool, bool) {
    (p.is_readable(), p.is_writable())
}

impl Split {
    /// `parts_per_page` parts, each `fill`.
    pub(crate) fn new(parts_per_page: usize, fill: Part) -> Self {
        debug_assert!(parts_per_page <= 64, "a part's bit is one of a u64's");
        Self { parts: vec![fill; parts_per_page], foreign: 0 }
    }

    /// Parts `[from, to)` as a mask of bits.
    pub(crate) fn mask(from: usize, to: usize) -> u64 {
        let below = |n: usize| if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        below(to) & !below(from)
    }

    /// Whether part `i` is [`foreign`](Self::foreign).
    pub(crate) fn is_foreign(&self, i: usize) -> bool {
        self.foreign >> i & 1 != 0
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
    /// too. `Mapped` parts and `StrictHole`s (as nothing) count; `Hole`s do not, and nor does a
    /// `Mapped(None)` part unless `strict_none` (the page is in a strict range, or
    /// `OMNI_STRICT_PROT_NONE=1`). With nothing counted, `None`.
    pub(crate) fn host_protection(&self, strict_none: bool) -> Protection {
        let mut counted = false;
        let (mut read, mut write) = (true, true);
        for part in &self.parts {
            let (r, w) = match *part {
                // A PROT_NONE part beside accessible ones: enforced only when strict (D42) -- a
                // library's reservation filler and a 4 KiB guard would otherwise make every access
                // to their host page's live part a trap.
                Part::Mapped(Protection::None) if !strict_none => continue,
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

    /// [`host_protection`](Self::host_protection), **lenient toward read-only parts beside a
    /// writable one** ([`lenient_read_only`]): on a host page that has a writable part, a
    /// `ReadExecute` part -- and a `Read` part that `sealed(i)` says is a library's sealed RELRO --
    /// does not forbid writes. The guest's own view keeps every part's protection; what changes is
    /// that a write to such a part, which a correct program never makes (code and RELRO are
    /// `mprotect`ed writable before they are written), is not refused -- while every access to the
    /// page's writable part stops trapping. A plain read-only part (a heap page a collector
    /// protects to catch writes) stays enforced. MEASURED (M4, 2026-10-16, `OMNI_SPLIT_REPORT`):
    /// Adobe AIR's JIT (Super Mechs) put code (`rx`) and data (`rw`) in one 16 KiB host page and its
    /// writes trapped ~10,800 times a second; libc's `.data` beside its RELRO (`r r r rw`) ~600 a
    /// second in each system process -- each trap a host fault and three Mach exceptions.
    pub(crate) fn host_protection_lenient(&self, strict_none: bool, sealed: impl Fn(usize) -> bool) -> Protection {
        if !self.parts.iter().any(|p| matches!(p, Part::Mapped(q) if q.is_writable())) {
            return self.host_protection(strict_none);
        }
        let relaxed = Split {
            parts: self
                .parts
                .iter()
                .enumerate()
                .map(|(i, p)| match *p {
                    Part::Mapped(Protection::ReadExecute) => Part::Mapped(Protection::ReadWrite),
                    Part::Mapped(Protection::Read) if sealed(i) => Part::Mapped(Protection::ReadWrite),
                    q => q,
                })
                .collect(),
            foreign: self.foreign,
        };
        relaxed.host_protection(strict_none)
    }

    /// Whether some `Mapped` part allows an access the host protection refuses: the page traps,
    /// and a served access needs its alias.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn traps(&self, strict_none: bool) -> bool {
        self.traps_under(self.host_protection(strict_none))
    }

    /// Whether some `Mapped` part allows an access host protection `host` refuses.
    pub(crate) fn traps_under(&self, host: Protection) -> bool {
        let host = bits(host);
        self.parts.iter().any(|part| match *part {
            Part::Mapped(p) => {
                let (r, w) = bits(p);
                (r && !host.0) || (w && !host.1)
            }
            Part::Hole | Part::StrictHole => false,
        })
    }

    /// The run of equal parts containing part `i`, foreign or not alike: (first index, count, part).
    pub(crate) fn run_at(&self, i: usize) -> (usize, usize, Part) {
        let key = |j: usize| (self.parts[j], self.is_foreign(j));
        let first = (0..i).rev().find(|&j| key(j) != key(i)).map_or(0, |j| j + 1);
        let last = (i..self.parts.len()).find(|&j| key(j) != key(i)).unwrap_or(self.parts.len());
        (first, last - first, self.parts[i])
    }

    /// Set parts `[from, to)` to `part`.
    pub(crate) fn set(&mut self, from: usize, to: usize, part: Part) {
        self.parts[from..to].fill(part);
    }
}

/// A space's overlay, present only when it is active (the space asked for [`GUEST_PAGE`]s, the
/// host page is larger, and the host can alias).
///
/// What the fault path reads (SUBPAGE-ORDER 2): `count` -- the number of trapping host pages,
/// zero almost always, which answers "no" with one atomic load -- and, only when it is not zero,
/// `aliases` under its read lock. Everything else is in `state`, taken only while the space's own
/// lock is held (SUBPAGE-ORDER 1). `aliases` is written only with `state` held and never held
/// while any other lock is taken.
pub(crate) struct SubPagesHandle {
    /// Trapping host pages: the number of `aliases` entries with `trapping` set.
    pub(crate) count: AtomicUsize,
    /// Host pages with a live alias, by host page address: where the alias is (placed by the host
    /// wherever it has room, one page of address space each -- a space may be terabytes), and
    /// whether the page traps now.
    pub(crate) aliases: RwLock<HashMap<usize, PageAlias>>,
    pub(crate) state: Mutex<SubPages>,
    /// Accesses the slow path served through an alias, for the report.
    pub(crate) served_total: AtomicU64,
}

/// A host page's alias, and whether the page traps.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageAlias {
    pub(crate) at: usize,
    pub(crate) trapping: bool,
}

/// The overlay's state: the tracked host pages and what else the guest asked.
pub(crate) struct SubPages {
    pub(crate) parts_per_page: usize,
    /// Host pages the guest has cut into differently-treated parts, by host page address.
    pub(crate) split: BTreeMap<usize, Split>,
    /// Host pages with a live alias (the keys of `SubPagesHandle::aliases`, in order, for range
    /// queries) -- trapping now, or once: an alias goes only with its page.
    pub(crate) aliased: BTreeSet<usize>,
    /// Guest ranges whose unmapped and PROT_NONE parts must fault (`[start, end)`).
    pub(crate) strict: Vec<(usize, usize)>,
    /// Guest ranges a library's RELRO was sealed read-only over (`[start, end)`): their read-only
    /// parts beside a writable part are lenient ([`Split::host_protection_lenient`]). Forgotten
    /// where they are unmapped or mapped over.
    pub(crate) sealed: Vec<(usize, usize)>,
    /// Accesses served, by host page.
    pub(crate) served: HashMap<usize, u64>,
}

impl SubPagesHandle {
    pub(crate) fn new(parts_per_page: usize) -> Self {
        Self {
            count: AtomicUsize::new(0),
            aliases: RwLock::new(HashMap::new()),
            state: Mutex::new(SubPages { parts_per_page, split: BTreeMap::new(), aliased: BTreeSet::new(), strict: Vec::new(), sealed: Vec::new(), served: HashMap::new() }),
            served_total: AtomicU64::new(0),
        }
    }

    /// Whether host page `host_page` traps. One atomic load when nothing traps (SUBPAGE-ORDER 2, 3).
    #[inline]
    pub(crate) fn is_trapping(&self, host_page: usize) -> bool {
        self.trapping_alias(host_page).is_some()
    }

    /// The alias of host page `host_page` if it traps.
    #[inline]
    pub(crate) fn trapping_alias(&self, host_page: usize) -> Option<usize> {
        if self.count.load(Ordering::Acquire) == 0 {
            return None;
        }
        self.aliases.read().get(&host_page).filter(|a| a.trapping).map(|a| a.at)
    }

    /// The alias of host page `host_page`, trapping or not.
    pub(crate) fn alias_of(&self, host_page: usize) -> Option<usize> {
        self.aliases.read().get(&host_page).map(|a| a.at)
    }

    /// A new alias for `host_page`, not trapping yet. `state` is held.
    pub(crate) fn insert_alias(&self, host_page: usize, at: usize) {
        self.aliases.write().insert(host_page, PageAlias { at, trapping: false });
    }

    /// `host_page`'s alias, gone from the map (its bit with it); the caller unmaps it. `state` is
    /// held.
    pub(crate) fn remove_alias(&self, host_page: usize) -> Option<usize> {
        let gone = self.aliases.write().remove(&host_page)?;
        if gone.trapping {
            self.count.fetch_sub(1, Ordering::Release);
        }
        Some(gone.at)
    }

    /// Set or clear `host_page`'s trapping, keeping `count`. Setting needs its alias made first
    /// (SUBPAGE-ORDER 3); clearing a page with no alias is nothing. `state` is held.
    pub(crate) fn set_trapping(&self, host_page: usize, on: bool) {
        let mut aliases = self.aliases.write();
        let Some(a) = aliases.get_mut(&host_page) else {
            debug_assert!(!on, "a page traps only with its alias made first");
            return;
        };
        if a.trapping != on {
            a.trapping = on;
            if on {
                self.count.fetch_add(1, Ordering::Release);
            } else {
                self.count.fetch_sub(1, Ordering::Release);
            }
        }
    }
}

/// `OMNI_SUBPAGE_LENIENT_RO=0`: read-execute parts and sealed RELRO beside a writable part forbid
/// writes on their host page as before ([`Split::host_protection_lenient`]); on by default.
pub(crate) fn lenient_read_only() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_SUBPAGE_LENIENT_RO").as_deref() != Ok("0"))
}

/// `OMNI_STRICT_PROT_NONE=1`: every PROT_NONE 4 KiB part is enforced, even beside accessible parts
/// of its host page (each access to those then traps). Off by default; per range, see
/// `GuestSpace::set_strict_gaps`.
pub(crate) fn strict_prot_none_everywhere() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_STRICT_PROT_NONE").as_deref() == Ok("1"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Protection::{Read, ReadExecute, ReadWrite, ReadWriteExecute};

    fn split(parts: &[Part]) -> Split {
        Split { parts: parts.to_vec(), foreign: 0 }
    }

    #[test]
    fn host_protection_is_the_least_any_counted_part_allows() {
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::Mapped(Read)]).host_protection(true), Read);
        assert_eq!(split(&[Part::Mapped(ReadExecute), Part::Mapped(ReadWrite)]).host_protection(true), Read);
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::Mapped(Protection::None)]).host_protection(true), Protection::None);
        assert_eq!(split(&[Part::Mapped(ReadWriteExecute), Part::Mapped(ReadWrite)]).host_protection(true), ReadWrite);
    }

    #[test]
    fn a_lenient_hole_does_not_count_and_a_strict_one_does() {
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::Hole]).host_protection(true), ReadWrite);
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::StrictHole]).host_protection(true), Protection::None);
        assert_eq!(split(&[Part::Hole, Part::Hole]).host_protection(true), Protection::None);
    }

    #[test]
    fn a_page_traps_only_when_a_mapped_part_allows_more_than_the_host() {
        assert!(!split(&[Part::Mapped(ReadWrite), Part::Hole]).traps(true));
        assert!(split(&[Part::Mapped(ReadWrite), Part::Mapped(Read)]).traps(true));
        assert!(!split(&[Part::Mapped(Protection::None), Part::StrictHole]).traps(true));
        assert!(split(&[Part::Mapped(Read), Part::StrictHole]).traps(true));
        // Execute alone never traps: guest code is fetched in software.
        assert!(!split(&[Part::Mapped(Read), Part::Mapped(ReadExecute)]).traps(true));
    }

    /// Beside a writable part, read-only and read-execute parts do not forbid writes on the host,
    /// and the page stops trapping; without a writable part nothing is relaxed.
    #[test]
    fn read_only_parts_beside_a_writable_one_are_lenient() {
        let none = |_| false;
        let jit = split(&[Part::Mapped(ReadExecute), Part::Mapped(ReadExecute), Part::Mapped(ReadExecute), Part::Mapped(ReadWrite)]);
        assert_eq!(jit.host_protection(false), Read);
        assert!(jit.traps(false), "strictly, the data part's writes trap");
        assert_eq!(jit.host_protection_lenient(false, none), ReadWrite);
        assert!(!jit.traps_under(jit.host_protection_lenient(false, none)), "leniently, nothing traps");
        let relro = split(&[Part::Mapped(Read), Part::Mapped(Read), Part::Mapped(Read), Part::Mapped(ReadWrite)]);
        assert_eq!(relro.host_protection_lenient(false, none), Read, "a read-only part not sealed RELRO is enforced");
        assert_eq!(relro.host_protection_lenient(false, |i| i < 3), ReadWrite, "sealed RELRO is lenient");
        assert_eq!(relro.host_protection_lenient(false, |i| i < 2), Read, "one part not sealed: enforced");
        let text = split(&[Part::Mapped(ReadExecute), Part::Mapped(Read)]);
        assert_eq!(text.host_protection_lenient(false, |_| true), Read, "no writable part: nothing relaxed");
        let guarded = split(&[Part::Mapped(ReadExecute), Part::Mapped(Protection::None), Part::Mapped(ReadWrite)]);
        assert_eq!(guarded.host_protection_lenient(true, none), Protection::None, "a strict PROT_NONE part still counts");
        assert_eq!(guarded.host_protection_lenient(false, none), ReadWrite);
    }

    #[test]
    fn a_prot_none_part_is_lenient_unless_strict() {
        let s = split(&[Part::Mapped(Read), Part::Mapped(Protection::None)]);
        assert_eq!(s.host_protection(false), Read, "lenient: the readable part does not trap");
        assert!(!s.traps(false));
        assert_eq!(s.host_protection(true), Protection::None, "strict: it faults, so the readable part traps");
        assert!(s.traps(true));
        let all_none = split(&[Part::Mapped(Protection::None), Part::Hole]);
        assert_eq!(all_none.host_protection(false), Protection::None, "nothing accessible: no access");
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

    /// A foreign part is a run of its own beside an equal part that is not; masks are exact.
    #[test]
    fn foreign_parts_are_runs_of_their_own() {
        let mut s = Split::new(4, Part::Mapped(Protection::None));
        s.foreign = Split::mask(0, 2);
        assert_eq!(s.run_at(1), (0, 2, Part::Mapped(Protection::None)));
        assert_eq!(s.run_at(2), (2, 2, Part::Mapped(Protection::None)));
        assert!(s.is_foreign(0) && !s.is_foreign(2));
        assert_eq!(Split::mask(1, 3), 0b110);
        assert_eq!(Split::mask(0, 64), u64::MAX);
        assert_eq!(Split::mask(2, 2), 0);
    }
}
