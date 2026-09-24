//! A per-thread memory of the region-map entries [`crate::admit`] and
//! [`GuestSpace::region_at`](crate::GuestSpace::region_at) looked up last, and the per-space
//! generation that says whether it is still true.
//!
//! # Why this exists
//!
//! MEASURED (Windows, in-world Pet Simulator 99, sampling profiler with symbols): on the render
//! thread and the busy workers, `GuestSpace::region_at` + `EntryMap::entry_start` +
//! `RawMutex::lock_slow` + `access::admit` were **28-35% of every sample outside translated code**,
//! the largest single cost in this workspace's own code. Every import handler's bounds check goes
//! through `admit` -- every `pthread_mutex_lock` reads the guest's mutex word through it -- and so
//! does `omni-cpu`'s instruction fetch; each one took the region map's one lock, and with ~60 guest
//! threads that lock was contended. MEASURED here (`tests/admit_cache.rs`, release, this machine,
//! before this module): 28-33 ns per `admit` on one thread, and **1.8-1.9 us per `admit`** with
//! eight threads hammering one mapping -- 4.2-4.5 M admits/s for the whole process. With it:
//! 10.9-11.1 ns on one thread, and 12.4-12.9 ns per `admit` with eight -- 467-523 M admits/s.
//!
//! The map changes rarely (a guest `mmap`/`munmap`/`mprotect`, a lazy commit, a reclaim) and is
//! read constantly, so the answer is to remember the last few answers per thread and to know,
//! with one load and no write, when they stopped being true.
//!
//! # The generation, and the ordering argument
//!
//! Each [`GuestSpace`](crate::GuestSpace) has a [`Generation`]: a process-unique `id` and a counter.
//! **Every** acquisition of the map lock for writing bumps the counter, **while holding the lock
//! and before touching anything** (`GuestSpace::write`) -- whether or not the operation then turns
//! out to change the map. Read-only paths take a guard that has no `DerefMut`, so a path that can
//! change the map without bumping does not compile.
//!
//! A slot remembered here was filled by a lookup made **under the lock**, and records the counter
//! value read **under that same lock**. Three facts make a hit exactly the locked answer:
//!
//! 1. *The map is constant between bumps, as any lock holder can see it.* A mutation happens only
//!    inside a write section, and a write section bumps first. So every locked read that sees
//!    counter value `G` sees the same map -- the one the write section that set `G` left behind --
//!    and a slot tagged `G` holds what any locked read at `G` would return.
//! 2. *A reader that loads `G` and hits uses the map as it was at `G`.* That is exactly what a
//!    locked read ordered before the next writer took the lock would have returned. A writer may be
//!    mid-mutation at that instant; its bump is not visible yet, so the reader is linearised before
//!    it. That is **the race the locked version already has**: `region_at` released the lock before
//!    its caller touched the memory, so a concurrent `munmap` could always land between the check
//!    and the access. Nothing new is admitted that the locked version could not also have admitted.
//! 3. *A reader that loads `G + 1` (or later) never uses a slot tagged `G`.* The tag comparison is
//!    exact; a stale slot is a miss and goes to the locked path, which refills it.
//!
//! The bump is `SeqCst`, before the writer's first OS call: a writer that decommits or unmaps pages
//! has published the new counter before the kernel is asked to change a single page table entry, so
//! a thread that trips over the change (a fault on a now-decommitted page, say) and then asks again
//! loads the new counter and goes to the lock. The reader's load is `Acquire`, which costs nothing
//! on x86 and one `ldar` on arm64, and **writes nothing**: the counter's cache line is only ever
//! written by a writer, so readers on every core keep it shared. [`Generation`] is aligned to 128
//! bytes so that the lock word next to it -- written on every lock acquisition -- does not share
//! that line.
//!
//! The counter is 64 bits: at a billion writes a second it wraps after 584 years.
//!
//! # What is remembered, and what a hit may answer
//!
//! Only **mapped** entries (free space is never remembered, so a refusal for an unmapped address
//! always takes the lock -- refusals are rare and are reported, so they are not the hot path). A
//! slot holds everything [`RegionInfo`] has **except a file's name**, which is an `Arc<str>` whose
//! clone would be a write to a shared reference count. So:
//!
//! * `admit` may answer from any slot, anonymous or file-backed (it never needs the name) -- but
//!   only for an access wholly inside the one entry, permitted by its protection, and with nothing
//!   for rule 4 to commit. See `access::admit` for why that is exactly the locked answer.
//! * `region_at` may answer only from an **anonymous** slot, which it can rebuild field for field.
//!   A file-backed region still takes the lock.
//!
//! # Several spaces
//!
//! Tests hold several spaces at once, and a space dropped and a new one reserved can land at the
//! same address. Slots are keyed by the space's `id`, which is never reused (a process-wide 64-bit
//! counter), so a slot can never answer for a space it was not filled from.
//!
//! # Reentrancy
//!
//! A slot is two `Cell` writes. The only code that can interrupt a thread and then call into this
//! module is the demand pager's fault handler, and it runs only for a fault on guest memory -- which
//! nothing in this module touches. That is the assumption the pager already makes about the map
//! lock itself (a fault handler that interrupted a lock holder would deadlock on it), so it is not a
//! new one; it is written down here because this is a second place that depends on it.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use omni_platform::vm::Protection;

use crate::region::{RegionInfo, RegionKind};
use crate::{GuestAddr, MappingId};

/// The per-space counter, and the identity that keys this thread's slots.
///
/// Aligned to 128 bytes -- two 64-byte lines, which is also the adjacent-line prefetch pair on x86
/// and the line size on Apple silicon -- so that nothing written often shares its line.
#[repr(align(128))]
pub(crate) struct Generation {
    id: u64,
    value: AtomicU64,
}

/// Where space ids come from. Starts at 1: 0 marks an empty slot.
static NEXT_SPACE_ID: AtomicU64 = AtomicU64::new(1);

impl Generation {
    pub(crate) fn new() -> Self {
        Self { id: NEXT_SPACE_ID.fetch_add(1, Ordering::Relaxed), value: AtomicU64::new(0) }
    }

    /// Record that the map is about to change. **Call with the map lock held, before the change.**
    ///
    /// `SeqCst` so that the new value is visible before any OS call the writer then makes; see the
    /// module documentation.
    pub(crate) fn bump(&self) {
        self.value.fetch_add(1, Ordering::SeqCst);
    }

    /// The counter, read **with the map lock held**: the value that tags whatever the caller reads
    /// from the map under the same lock. Relaxed is enough, because every bump happens under the
    /// lock and the lock's own acquire orders it before this.
    pub(crate) fn locked(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    /// The counter, read **without** the lock, to decide whether a slot is still true.
    fn current(&self) -> u64 {
        self.value.load(Ordering::Acquire)
    }
}

/// How many entries each thread remembers.
///
/// A handler's working set is small -- the guest thread's stack, a heap arena or two, a library's
/// `.data` and `.bss` -- and a lookup scans every key, so this is a handful and not a table.
const SLOTS: usize = 8;

#[derive(Clone, Copy)]
struct Key {
    /// [`Generation::id`] of the space the slot came from; 0 for an empty slot.
    space: u64,
    generation: u64,
    start: GuestAddr,
    end: GuestAddr,
}

const EMPTY: Key = Key { space: 0, generation: 0, start: 0, end: 0 };

/// One remembered entry: [`RegionInfo`] without the name of a file. See the module documentation
/// for why the name is the one field left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Remembered {
    pub(crate) start: GuestAddr,
    pub(crate) len: usize,
    pub(crate) protection: Protection,
    pub(crate) anonymous: bool,
    pub(crate) committed: usize,
    mapping: MappingId,
    mapping_start: GuestAddr,
    mapping_len: usize,
}

impl Remembered {
    pub(crate) fn end(&self) -> GuestAddr {
        self.start + self.len
    }

    /// Whether the entry is committed end to end: [`RegionInfo::is_committed`], field for field.
    pub(crate) fn is_committed(&self) -> bool {
        self.committed == self.len
    }

    /// The [`RegionInfo`] the locked lookup built this from, when it can be rebuilt exactly: for an
    /// anonymous entry. A file-backed one would need the file's name.
    pub(crate) fn anonymous_region(&self) -> Option<RegionInfo> {
        self.anonymous.then(|| RegionInfo {
            start: self.start,
            len: self.len,
            protection: self.protection,
            kind: RegionKind::Anonymous,
            committed: self.committed,
            mapping: Some(self.mapping),
            mapping_start: self.mapping_start,
            mapping_len: self.mapping_len,
        })
    }

    fn of(region: &RegionInfo) -> Option<Self> {
        let anonymous = match region.kind {
            RegionKind::Free => return None,
            RegionKind::Anonymous => true,
            RegionKind::File { .. } => false,
        };
        Some(Self {
            start: region.start,
            len: region.len,
            protection: region.protection,
            anonymous,
            committed: region.committed,
            mapping: region.mapping?,
            mapping_start: region.mapping_start,
            mapping_len: region.mapping_len,
        })
    }
}

const PLACEHOLDER: Remembered = Remembered {
    start: 0,
    len: 0,
    protection: Protection::None,
    anonymous: false,
    committed: 0,
    mapping: MappingId(0),
    mapping_start: 0,
    mapping_len: 0,
};

struct Slots {
    keys: [Cell<Key>; SLOTS],
    entries: [Cell<Remembered>; SLOTS],
    /// Round-robin victim for a fill that finds no stale or empty slot.
    next: Cell<usize>,
    /// Answers this thread's `admit` and `region_at` gave from a slot, without the lock. Diagnostics,
    /// and the witnesses the tests use to show that each fast path is taken at all -- a fast path that
    /// silently stopped being taken would still pass every test of what it answers. Thread-local, so
    /// counting writes nothing another core reads.
    admits: Cell<u64>,
    regions: Cell<u64>,
}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_KEY: Cell<Key> = Cell::new(EMPTY);
#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_ENTRY: Cell<Remembered> = Cell::new(PLACEHOLDER);

thread_local! {
    // `const`-initialised and free of `Drop`, so a thread's first use allocates nothing and
    // registers no destructor -- which matters because the demand pager's fault handler is one of
    // the callers.
    static SLOTS_OF_THIS_THREAD: Slots = const {
        Slots {
            keys: [EMPTY_KEY; SLOTS],
            entries: [EMPTY_ENTRY; SLOTS],
            next: Cell::new(0),
            admits: Cell::new(0),
            regions: Cell::new(0),
        }
    };
}

/// The entry of `generation`'s space that contains `address`, if this thread remembers it **and it
/// is still true**. No lock, and no write to anything another thread reads.
#[inline]
pub(crate) fn lookup(generation: &Generation, address: GuestAddr) -> Option<Remembered> {
    let current = generation.current();
    SLOTS_OF_THIS_THREAD.with(|slots| {
        for (key, entry) in slots.keys.iter().zip(&slots.entries) {
            let key = key.get();
            if key.space == generation.id
                && key.generation == current
                && address >= key.start
                && address < key.end
            {
                return Some(entry.get());
            }
        }
        None
    })
}

/// Remember `region`, which a lookup of `generation`'s space read under the map lock when the
/// counter was `at` ([`Generation::locked`], read under that same lock). Free space is not
/// remembered.
pub(crate) fn remember(generation: &Generation, at: u64, region: &RegionInfo) {
    let Some(entry) = Remembered::of(region) else {
        return;
    };
    let key = Key { space: generation.id, generation: at, start: entry.start, end: entry.end() };
    SLOTS_OF_THIS_THREAD.with(|slots| {
        // The same entry again, or a slot that can never hit again (empty, or this space's from an
        // older generation), before evicting anything that might still be true.
        let reusable = slots.keys.iter().position(|slot| {
            let slot = slot.get();
            slot.space == 0
                || (slot.space == key.space
                    && (slot.generation != key.generation || slot.start == key.start))
        });
        let index = reusable.unwrap_or_else(|| {
            let next = slots.next.get();
            slots.next.set((next + 1) % SLOTS);
            next
        });
        // Emptied first and keyed last, so a slot is never keyed with another entry's contents.
        slots.keys[index].set(EMPTY);
        slots.entries[index].set(entry);
        slots.keys[index].set(key);
    });
}

/// Which fast path answered.
#[derive(Clone, Copy)]
pub(crate) enum Answered {
    Admit,
    Region,
}

/// Count one answer given from a slot, on this thread.
#[inline]
pub(crate) fn answered(by: Answered) {
    SLOTS_OF_THIS_THREAD.with(|slots| {
        let counter = match by {
            Answered::Admit => &slots.admits,
            Answered::Region => &slots.regions,
        };
        counter.set(counter.get().wrapping_add(1));
    });
}

/// How many answers this thread's `admit` and `region_at` have given from a slot, over every space.
pub(crate) fn answers() -> (u64, u64) {
    SLOTS_OF_THIS_THREAD.with(|slots| (slots.admits.get(), slots.regions.get()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anonymous(start: GuestAddr, len: usize) -> RegionInfo {
        RegionInfo {
            start,
            len,
            protection: Protection::ReadWrite,
            kind: RegionKind::Anonymous,
            committed: len,
            mapping: Some(MappingId(7)),
            mapping_start: start,
            mapping_len: len,
        }
    }

    /// A slot answers only for the space it was filled from, at the generation it was filled at.
    ///
    /// Driven here rather than through two live spaces because the collision it guards against --
    /// a space dropped and a new one reserved at the same base, whose counter has reached the same
    /// value -- depends on where the OS puts a reservation, which a test cannot arrange.
    #[test]
    fn a_slot_answers_only_for_its_own_space_and_generation() {
        let one = Generation::new();
        let two = Generation::new();
        assert_ne!(one.id, two.id, "two spaces share an id");
        let region = anonymous(0x10_0000, 0x1_0000);
        remember(&one, one.locked(), &region);

        assert_eq!(lookup(&one, 0x10_0000).and_then(|r| r.anonymous_region()), Some(region.clone()));
        assert_eq!(lookup(&one, 0x10_ffff).map(|r| r.start), Some(0x10_0000));
        assert_eq!(lookup(&one, 0x11_0000), None, "one past the end");
        assert_eq!(lookup(&one, 0x0f_ffff), None, "one before the start");
        // Same counter value (both are at 0), same address, different space.
        assert_eq!(two.locked(), one.locked());
        assert_eq!(lookup(&two, 0x10_0000), None, "a slot answered for another space");

        one.bump();
        assert_eq!(lookup(&one, 0x10_0000), None, "a slot outlived a bump");
    }

    /// Free space is never remembered, and a file-backed entry is remembered for `admit` but cannot
    /// be rebuilt as a `RegionInfo`.
    #[test]
    fn free_space_is_not_remembered_and_a_file_region_is_not_rebuilt() {
        let space = Generation::new();
        let free = RegionInfo {
            kind: RegionKind::Free,
            mapping: None,
            committed: 0,
            protection: Protection::None,
            ..anonymous(0x20_0000, 0x1000)
        };
        remember(&space, space.locked(), &free);
        assert_eq!(lookup(&space, 0x20_0000), None);

        let file = RegionInfo {
            kind: RegionKind::File {
                backing: crate::backing::BackingId(1),
                name: "lib.so".into(),
                file_offset: 0,
                shared: false,
                guest_named: false,
            },
            committed: 0,
            ..anonymous(0x30_0000, 0x1000)
        };
        remember(&space, space.locked(), &file);
        let hit = lookup(&space, 0x30_0000).expect("a file region is remembered");
        assert!(!hit.anonymous && !hit.is_committed());
        assert_eq!(hit.anonymous_region(), None);
    }

    /// Filling more entries than there are slots evicts, and never leaves a slot keyed with another
    /// entry's contents.
    #[test]
    fn eviction_keeps_every_key_with_its_own_entry() {
        let space = Generation::new();
        let at = space.locked();
        for i in 0..(SLOTS * 3) {
            remember(&space, at, &anonymous(0x100_0000 + i * 0x1_0000, 0x1_0000));
        }
        let mut found = 0;
        for i in 0..(SLOTS * 3) {
            let start = 0x100_0000 + i * 0x1_0000;
            if let Some(hit) = lookup(&space, start + 5) {
                assert_eq!(hit.start, start, "a key answered with another entry");
                found += 1;
            }
        }
        assert_eq!(found, SLOTS, "every slot should hold one of the most recent entries");
    }
}
