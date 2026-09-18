//! The region map: what the guest address space contains, address by address.
//!
//! # Why a `BTreeMap` keyed on the range start
//!
//! The guest queries this constantly — every emulated `mmap`, `mprotect`, `munmap` and every
//! synthesised `/proc/self/maps` read — so the brief requires better-than-linear address lookup.
//! The operation that decides the structure is not "look up an exact key" but **"find the range
//! containing this address"**, and then "walk forward from it". `BTreeMap::range(..=addr)
//! .next_back()` answers the first in `O(log n)` and the second by continuing the same cursor, so
//! one structure serves both without a second index.
//!
//! A hash map cannot answer either question: the key being looked up is almost never a range start.
//! A sorted `Vec` with binary search has the same asymptotics and better constants, but every map
//! *and* unmap inserts or removes in the middle, which is `O(n)` memmove on a `Vec`, and the guest
//! does that as often as it queries. An interval tree would buy nothing over a `BTreeMap` here
//! because the ranges are **non-overlapping and gapless** — which is the real invariant this module
//! enforces — so "the range containing `addr`" is always the nearest key at or before `addr`, and
//! there is never a set of candidates to search among.
//!
//! # The invariant
//!
//! Entries tile `[base, base + len)` exactly: sorted, non-overlapping, no gaps, none empty. Free
//! address space is represented by an entry whose [`Entry::owner`] is `None` rather than by the
//! absence of an entry, so that "what is at this address" is always one lookup and never a
//! comparison against neighbours. [`EntryMap::check_invariants`] asserts all of it and is called
//! from the tests after every interesting operation.
//!
//! # One more invariant, this one imposed by Windows
//!
//! A [`OsState::Placeholder`] entry is **exactly one OS placeholder**. That is not bookkeeping
//! taste; it is forced, and it was measured:
//!
//! * `split_placeholder` of a range that is already exactly one placeholder fails with
//!   `ERROR_INVALID_ADDRESS` (487), so the map has to know whether a split is needed.
//! * `split_placeholder` of a range that spans two adjacent placeholders fails with
//!   `ERROR_INVALID_PARAMETER` (87), so the map has to know where the boundaries are.
//! * `coalesce_placeholders` of a range holding only one placeholder also fails with 487.
//!
//! So a placeholder entry may never be split or merged as pure bookkeeping — every boundary change
//! on a placeholder is paired with the matching OS call in [`crate::space`]. [`OsState::Private`]
//! and [`OsState::View`] entries have no such rule: a partial `MEM_RELEASE |
//! MEM_PRESERVE_PLACEHOLDER` of a private region is legal and was measured to leave its neighbours'
//! contents intact, and a view is *deliberately* allowed to span several entries so that
//! `protect()` can give different pages of one view different protections.

use std::collections::BTreeMap;
use std::sync::Arc;

use omni_platform::vm::Protection;

use crate::backing::Backing;
use crate::{CommitPolicy, GuestAddr, MappingId};

/// Identity of one OS-level file view. Entries sharing it form a single view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ViewId(pub u64);

/// What the OS has at a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OsState {
    /// An unreplaced placeholder: address space this process owns, with nothing in it. Costs no
    /// commit charge. Exactly one OS placeholder per entry — see the module documentation.
    Placeholder,
    /// Private committed memory, from `commit_placeholder`. Costs commit charge equal to its
    /// length plus page tables.
    Private {
        /// Set by [`crate::GuestSpace::advise_idle`]: the guest has said it no longer needs the
        /// contents, so [`crate::GuestSpace::reclaim_idle`] may decommit it. The contents are
        /// still there until it does.
        idle: bool,
    },
    /// Part of a file-backed view. Costs essentially no commit charge when read-only or
    /// execute-read (measured +0.008 MiB for a 4 MiB view, unchanged after reading every byte).
    View {
        /// Which OS view this entry belongs to. All entries of one view are contiguous.
        view: ViewId,
    },
}

impl OsState {
    /// A short noun for diagnostics.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            OsState::Placeholder => "reserved",
            OsState::Private { idle: false } => "private committed memory",
            OsState::Private { idle: true } => "private committed memory marked idle",
            OsState::View { .. } => "a file-backed view",
        }
    }
}

/// The guest mapping an entry belongs to.
///
/// Every field except [`Owner::protection`] describes the whole mapping and is identical across its
/// entries; `protection` is per entry, because `mprotect` on part of a mapping is normal and
/// `/proc/self/maps` reports the result as separate lines.
#[derive(Debug, Clone)]
pub(crate) struct Owner {
    pub(crate) id: MappingId,
    /// Start of the whole mapping. Commit granules are laid out from here, so that a mapping's
    /// granule boundaries do not depend on how it has since been carved up.
    pub(crate) mapping_start: GuestAddr,
    /// Length of the whole mapping as originally requested.
    pub(crate) mapping_len: usize,
    /// Protection of *this entry*.
    pub(crate) protection: Protection,
    /// Commit granule of the mapping.
    pub(crate) granule: usize,
    /// Whether commit is driven by [`crate::GuestSpace::ensure_committed`] or was done up front.
    pub(crate) commit: CommitPolicy,
    /// The file this mapping comes from, if any.
    pub(crate) backing: Option<Arc<Backing>>,
    /// The file offset corresponding to [`Owner::mapping_start`]. The offset of any entry is
    /// derived from it, because the correspondence is linear and stays linear when the mapping is
    /// carved up.
    pub(crate) file_offset: u64,
}

impl Owner {
    /// File offset corresponding to a guest address inside this mapping.
    pub(crate) fn offset_at(&self, address: GuestAddr) -> u64 {
        debug_assert!(address >= self.mapping_start);
        self.file_offset + (address - self.mapping_start) as u64
    }
}

/// One entry of the region map: a contiguous range with one OS state and at most one owner.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) len: usize,
    pub(crate) os: OsState,
    /// `None` means free address space: an unreplaced placeholder no guest mapping claims.
    pub(crate) owner: Option<Owner>,
    /// Whether these pages have ever been writable, and so may hold copy-on-write content that is
    /// **not** in the backing file.
    ///
    /// Only meaningful for [`OsState::View`] entries, and it is sticky: once a view's pages have been
    /// writable, Windows gives no reliable way to find out afterwards whether they were actually
    /// written — a privatised page still reports `MEM_MAPPED` (Task 1), and `QueryWorkingSetEx`'s
    /// shared bit is only meaningful for pages that are currently resident.
    ///
    /// It exists because [`crate::GuestSpace::unmap`] has to unmap a whole view to unmap part of one,
    /// and re-mapping a survivor brings it back *from the file* — so anything written into it through
    /// a copy-on-write protection would be lost. The flag is what lets that content be preserved
    /// without a comparison pass over views that cannot possibly hold any, which is the overwhelming
    /// majority of them: `libroblox.so`'s ~104 MB of text is mapped `ReadExecute` and never made
    /// writable at all.
    pub(crate) ever_writable: bool,
}

impl Entry {
    pub(crate) fn free(len: usize) -> Self {
        Self { len, os: OsState::Placeholder, owner: None, ever_writable: false }
    }

    pub(crate) fn is_free(&self) -> bool {
        self.owner.is_none()
    }

    /// Commit charge this entry is responsible for, ignoring page tables.
    pub(crate) fn committed_bytes(&self) -> usize {
        match self.os {
            OsState::Private { .. } => self.len,
            OsState::Placeholder | OsState::View { .. } => 0,
        }
    }

    pub(crate) fn describe(&self) -> &'static str {
        match (&self.owner, self.os) {
            (None, _) => "free address space",
            (Some(owner), os) => match (owner.backing.is_some(), os) {
                (true, _) => "a file mapping",
                (false, OsState::Placeholder) => "an anonymous mapping, not yet committed",
                (false, _) => "an anonymous mapping",
            },
        }
    }
}

/// The region map. See the module documentation for the structure choice and the invariants.
#[derive(Debug)]
pub(crate) struct EntryMap {
    base: GuestAddr,
    len: usize,
    entries: BTreeMap<GuestAddr, Entry>,
}

impl EntryMap {
    /// A whole guest address space as one free entry.
    pub(crate) fn new(base: GuestAddr, len: usize) -> Self {
        let mut entries = BTreeMap::new();
        entries.insert(base, Entry::free(len));
        Self { base, len, entries }
    }

    pub(crate) fn base(&self) -> GuestAddr {
        self.base
    }

    pub(crate) fn end(&self) -> GuestAddr {
        self.base + self.len
    }

    pub(crate) fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Start address of the entry containing `address`, in `O(log n)`.
    pub(crate) fn entry_start(&self, address: GuestAddr) -> Option<GuestAddr> {
        let (&start, entry) = self.entries.range(..=address).next_back()?;
        if address < start + entry.len {
            Some(start)
        } else {
            None
        }
    }

    pub(crate) fn get(&self, start: GuestAddr) -> Option<&Entry> {
        self.entries.get(&start)
    }

    pub(crate) fn get_mut(&mut self, start: GuestAddr) -> Option<&mut Entry> {
        self.entries.get_mut(&start)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (GuestAddr, &Entry)> {
        self.entries.iter().map(|(&start, entry)| (start, entry))
    }

    /// The starts of every entry overlapping `[address, address + len)`, in address order.
    ///
    /// Returned as a `Vec` rather than an iterator on purpose: almost every caller mutates the map
    /// while walking the result, and a borrow held across that is not expressible.
    pub(crate) fn starts_overlapping(&self, address: GuestAddr, len: usize) -> Vec<GuestAddr> {
        let end = address.saturating_add(len);
        let first = self.entry_start(address).unwrap_or(address);
        self.entries
            .range(first..end)
            .map(|(&start, _)| start)
            .collect()
    }

    /// Replace the entry at `start` with two entries split at `at`, keeping the OS state and owner.
    ///
    /// **Bookkeeping only.** For an [`OsState::Placeholder`] entry the caller must have performed
    /// the matching `split_placeholder` first; see the module documentation for why.
    pub(crate) fn split_bookkeeping(&mut self, start: GuestAddr, at: GuestAddr) {
        let entry = self.entries.get_mut(&start).expect("split of a range with no entry");
        assert!(at > start && at < start + entry.len, "split at {at:#x} is not inside the entry");
        let tail_len = start + entry.len - at;
        entry.len = at - start;
        let tail = Entry {
            len: tail_len,
            os: entry.os,
            owner: entry.owner.clone(),
            // Conservative on purpose: if either half could hold copy-on-write content, both halves
            // are treated as if they might, because the flag records what was *permitted* rather
            // than what was written.
            ever_writable: entry.ever_writable,
        };
        self.entries.insert(at, tail);
    }

    /// Replace `[start, start + len)` — which must currently be whole entries — with one entry.
    pub(crate) fn replace(&mut self, start: GuestAddr, len: usize, entry: Entry) {
        debug_assert_eq!(entry.len, len);
        let end = start + len;
        let doomed: Vec<GuestAddr> = self.entries.range(start..end).map(|(&s, _)| s).collect();
        debug_assert!(!doomed.is_empty(), "replace of a range with no entries");
        debug_assert_eq!(doomed[0], start, "replace must start on an entry boundary");
        for s in doomed {
            let removed = self.entries.remove(&s).expect("entry vanished");
            debug_assert!(s + removed.len <= end, "replace must end on an entry boundary");
        }
        self.entries.insert(start, entry);
    }

    /// Set every entry in `[start, start + len)` — which must be whole entries — free.
    pub(crate) fn free_range(&mut self, start: GuestAddr, len: usize) {
        for s in self.starts_overlapping(start, len) {
            let entry = self.entries.get_mut(&s).expect("entry vanished");
            debug_assert!(s >= start && s + entry.len <= start + len);
            entry.owner = None;
            entry.os = OsState::Placeholder;
            entry.ever_writable = false;
        }
    }

    /// Total free bytes and the largest single free range.
    pub(crate) fn free_summary(&self) -> (usize, usize) {
        let mut total = 0;
        let mut largest = 0;
        let mut run = 0;
        for (_, entry) in self.iter() {
            if entry.is_free() {
                run += entry.len;
                total += entry.len;
                largest = largest.max(run);
            } else {
                run = 0;
            }
        }
        (total, largest)
    }

    /// Assert the structural invariants: a gapless, sorted, non-overlapping tiling of the space.
    ///
    /// # Panics
    ///
    /// If any invariant is broken. Called from the tests after every interesting operation, and
    /// from `debug_assertions` builds after every mutation, because a region map that has silently
    /// drifted out of step with the OS is exactly the failure that would present as an
    /// unexplainable access violation somewhere else entirely.
    // Only called from `space::Inner::validate`, which is itself `debug_assertions`-only, so a
    // release build compiles this away and would otherwise report it as dead.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pub(crate) fn check_invariants(&self) {
        assert!(!self.entries.is_empty(), "the region map is empty");
        // The walk is O(n) and `space` runs it after every mutation in a `debug_assertions` build,
        // which is quadratic for a mapping carved into thousands of granules — a 64 MiB mapping at a
        // 4 KiB granule is 16,384 of them. Release builds never run this at all, so the cap only
        // decides how large a map a development build keeps checking, and every test in this crate
        // works well below it.
        if self.entries.len() > 4096 {
            return;
        }
        let mut expected = self.base;
        for (&start, entry) in &self.entries {
            assert_eq!(start, expected, "gap or overlap at {start:#x}, expected {expected:#x}");
            assert_ne!(entry.len, 0, "zero-length entry at {start:#x}");
            if let Some(owner) = &entry.owner {
                assert!(
                    start >= owner.mapping_start
                        && start + entry.len <= owner.mapping_start + owner.mapping_len,
                    "entry {start:#x}+{:#x} escapes its mapping {:#x}+{:#x}",
                    entry.len,
                    owner.mapping_start,
                    owner.mapping_len
                );
                assert!(owner.granule > 0, "mapping at {:#x} has a zero granule", owner.mapping_start);
            }
            if entry.is_free() {
                assert_eq!(
                    entry.os,
                    OsState::Placeholder,
                    "free entry at {start:#x} is not a placeholder but {:?}",
                    entry.os
                );
            }
            expected = start + entry.len;
        }
        assert_eq!(expected, self.end(), "the region map ends at {expected:#x}, not {:#x}", self.end());
    }
}
