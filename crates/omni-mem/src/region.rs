//! The region enumeration: what the guest would see if it read its own `/proc/self/maps`.

use std::sync::Arc;

use omni_platform::vm::Protection;

use crate::backing::BackingId;
use crate::entry::{Entry, OsState};
use crate::{GuestAddr, MappingId};

/// What a region is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionKind {
    /// Address space this guest space owns with nothing mapped in it. Reported so that a consumer
    /// can see the whole space accounted for; `/proc/self/maps` would simply omit these.
    Free,
    /// Anonymous memory, the guest's `mmap(MAP_ANONYMOUS)`.
    Anonymous,
    /// Memory mapped from a file.
    File {
        /// Which file, as an identity that is cheap to compare.
        backing: BackingId,
        /// The file's name, which is what the last column of a `/proc/self/maps` line holds.
        name: Arc<str>,
        /// File offset of the start of this region, which is the fourth column.
        file_offset: u64,
        /// Whether the mapping writes the file (`MAP_SHARED`): the `s` rather than the `p` in the
        /// permission column.
        shared: bool,
        /// Whether `name` is a guest path rather than a host one: see
        /// [`Backing::is_guest_named`](crate::Backing::is_guest_named). A host path must never
        /// reach the guest, and on a unix host its shape does not tell the two apart.
        guest_named: bool,
    },
}

/// One region of a guest address space.
///
/// Adjacent ranges the guest cannot distinguish are merged into one of these, so the list maps
/// one-to-one onto the lines of `/proc/self/maps`: [`start`](RegionInfo::start) and
/// [`end`](RegionInfo::end) are the address column, [`protection`](RegionInfo::protection) is the
/// permission column (the private/shared flag follows from [`kind`](RegionInfo::kind)), and a
/// [`RegionKind::File`] carries the offset and the pathname columns. The device and inode columns
/// have no meaning for a synthesized map and are the synthesizer's problem, not this crate's.
///
/// [`mapping_start`](RegionInfo::mapping_start) and [`mapping_len`](RegionInfo::mapping_len)
/// describe the *whole* mapping this region is part of, which is what `dl_iterate_phdr` needs: a
/// loaded object's base address has to stay knowable after `mprotect` has split its segments into
/// several lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionInfo {
    /// Start address.
    pub start: GuestAddr,
    /// Length in bytes.
    pub len: usize,
    /// Protection of this region. Every page in it has this protection.
    pub protection: Protection,
    /// What is mapped here.
    pub kind: RegionKind,
    /// Bytes of private commit charge this region is responsible for. Zero for free and
    /// file-backed regions, and zero for the uncommitted granules of a lazy mapping — which is what
    /// makes the difference between a region existing and a region costing anything.
    pub committed: usize,
    /// Which guest mapping this region belongs to, or `None` for free space.
    pub mapping: Option<MappingId>,
    /// Start of the whole mapping this region is part of.
    pub mapping_start: GuestAddr,
    /// Length of the whole mapping this region is part of.
    pub mapping_len: usize,
}

impl RegionInfo {
    /// End address, exclusive.
    #[must_use]
    pub fn end(&self) -> GuestAddr {
        self.start + self.len
    }

    /// Whether this region is free address space.
    #[must_use]
    pub fn is_free(&self) -> bool {
        self.kind == RegionKind::Free
    }

    /// Whether this region is entirely committed. False for a file-backed region, whose pages cost
    /// no commit charge at all.
    #[must_use]
    pub fn is_committed(&self) -> bool {
        self.committed == self.len
    }

    pub(crate) fn from_entry(start: GuestAddr, entry: &Entry) -> Self {
        match &entry.owner {
            None => Self {
                start,
                len: entry.len,
                protection: Protection::None,
                kind: RegionKind::Free,
                committed: 0,
                mapping: None,
                mapping_start: start,
                mapping_len: entry.len,
            },
            Some(owner) => {
                let kind = match (&owner.backing, entry.os) {
                    (Some(backing), _) => RegionKind::File {
                        backing: backing.id(),
                        name: Arc::clone(backing.name()),
                        file_offset: owner.offset_at(start),
                        shared: backing.is_shared(),
                        guest_named: backing.is_guest_named(),
                    },
                    (None, _) => RegionKind::Anonymous,
                };
                Self {
                    start,
                    len: entry.len,
                    protection: owner.protection,
                    kind,
                    committed: match entry.os {
                        OsState::Private { .. } => entry.len,
                        OsState::Placeholder | OsState::View { .. } => 0,
                    },
                    mapping: Some(owner.id),
                    mapping_start: owner.mapping_start,
                    mapping_len: owner.mapping_len,
                }
            }
        }
    }

    /// Whether `next` is the immediate continuation of this region, in every respect the guest can
    /// observe.
    ///
    /// Commit state is deliberately *not* one of those respects: whether a granule of a lazy
    /// mapping happens to be committed is invisible to the guest, and splitting a line of
    /// `/proc/self/maps` on it would leak Omnidroid's internal accounting into something the guest
    /// parses. The bytes are still accounted, in [`RegionInfo::committed`].
    pub(crate) fn can_absorb(&self, next: &RegionInfo) -> bool {
        if self.end() != next.start || self.protection != next.protection {
            return false;
        }
        match (&self.kind, &next.kind) {
            (RegionKind::Free, RegionKind::Free) => true,
            (RegionKind::Anonymous, RegionKind::Anonymous) => self.mapping == next.mapping,
            (
                RegionKind::File { backing, file_offset, .. },
                RegionKind::File { backing: next_backing, file_offset: next_offset, .. },
            ) => {
                self.mapping == next.mapping
                    && backing == next_backing
                    && *next_offset == *file_offset + self.len as u64
            }
            _ => false,
        }
    }

    pub(crate) fn absorb(&mut self, next: &RegionInfo) {
        debug_assert!(self.can_absorb(next));
        self.len += next.len;
        self.committed += next.committed;
    }
}
