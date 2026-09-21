//! Opaque NDK handles: a fixed set of arena slots, and the checked map from a guest pointer back
//! to what it names.
//!
//! # Why every NDK object is an arena address
//!
//! `ALooper`, `AAssetManager`, `AAsset` and `AConfiguration` are all **opaque** — the NDK headers
//! declare each as a bare `struct X;` and never define it, and the GameActivity glue stores the
//! pointer and hands it back. So what this layer owes the guest is an identity it can compare and
//! store, not a layout.
//!
//! Making that identity a **real guest address** rather than a small integer costs one page and
//! buys two things. A guest that dereferences one reads a recognisable magic in a dump instead of
//! faulting on an address that was never mapped; and a pointer from a *different* instance of this
//! runtime is outside this arena, so it is refused rather than silently indexing a table it does
//! not belong to.
//!
//! # The check is the point
//!
//! [`Slots::index_of`] refuses a pointer that is inside the region but **not on a slot boundary**.
//! Without that, `(at - base) / SLOT_BYTES` turns any address in the region into a valid index —
//! so a guest that computed `looper + 4` would operate on the looper, and a guest that computed
//! `looper + 20` would operate on the next one. This is the same reasoning `jni::refs` gives for
//! checking a `jobject` rather than trusting it: the guest is hostile by assumption (D6).

use omni_mem::GuestAddr;

/// Bytes of arena each opaque NDK handle occupies.
///
/// Sixteen rather than eight so that a slot can hold its magic **and** its index, which is what
/// makes a dump of the arena readable. Nothing reads either back: the address is the identity.
pub const SLOT_BYTES: usize = 16;

/// What an unused slot holds, for a reader of a memory dump. Nothing reads it back.
pub const SLOT_MAGIC: u64 = 0x004F_4D4E_4E44_4B00; // "\0OMNNDK\0"

/// A fixed-capacity table of one kind of opaque handle, addressed by arena slot.
#[derive(Debug)]
pub(super) struct Slots<T> {
    base: GuestAddr,
    entries: Vec<Option<T>>,
}

impl<T> Slots<T> {
    /// A table of `capacity` slots starting at `base`.
    pub(super) fn new(base: GuestAddr, capacity: usize) -> Slots<T> {
        let mut entries = Vec::with_capacity(capacity);
        entries.resize_with(capacity, || None);
        Slots { base, entries }
    }

    /// One past the last address this table covers.
    pub(super) fn end(&self) -> GuestAddr {
        self.base + self.entries.len() * SLOT_BYTES
    }

    /// The guest address of slot `index`.
    pub(super) fn address_of(&self, index: usize) -> GuestAddr {
        self.base + index * SLOT_BYTES
    }

    /// The slot a guest pointer names, **checked**: in range, and on a slot boundary.
    pub(super) fn index_of(&self, at: GuestAddr) -> Option<usize> {
        if at < self.base || at >= self.end() {
            return None;
        }
        let offset = at - self.base;
        (offset % SLOT_BYTES == 0).then_some(offset / SLOT_BYTES)
    }

    /// Put `value` in the lowest free slot and return its address.
    pub(super) fn insert(&mut self, value: T) -> Option<GuestAddr> {
        let index = self.entries.iter().position(Option::is_none)?;
        self.entries[index] = Some(value);
        Some(self.address_of(index))
    }

    /// What a live guest pointer names.
    pub(super) fn get(&self, at: GuestAddr) -> Option<&T> {
        self.index_of(at).and_then(|index| self.entries[index].as_ref())
    }

    /// What a live guest pointer names, mutably.
    pub(super) fn get_mut(&mut self, at: GuestAddr) -> Option<&mut T> {
        self.index_of(at).and_then(move |index| self.entries[index].as_mut())
    }

    /// Free the slot a guest pointer names, returning what was there.
    pub(super) fn remove(&mut self, at: GuestAddr) -> Option<T> {
        self.index_of(at).and_then(|index| self.entries[index].take())
    }

    /// How many slots hold something.
    pub(super) fn live(&self) -> usize {
        self.entries.iter().filter(|slot| slot.is_some()).count()
    }

    /// Every live entry, with its address.
    pub(super) fn iter(&self) -> impl Iterator<Item = (GuestAddr, &T)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|value| (self.address_of(index), value)))
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A pointer inside the region but off a slot boundary is refused.**
    ///
    /// The whole reason `index_of` is a function rather than a division: without the alignment
    /// test, `base + 4` and `base + 20` both name a slot, and a guest that computed either would
    /// operate on an object it does not hold.
    #[test]
    fn a_pointer_off_a_slot_boundary_names_nothing() {
        let mut slots: Slots<u32> = Slots::new(0x1_0000, 4);
        let first = slots.insert(7).expect("a free slot");
        assert_eq!(first, 0x1_0000);
        assert_eq!(slots.get(first).copied(), Some(7));
        for offset in 1..SLOT_BYTES {
            assert_eq!(
                slots.index_of(first + offset),
                None,
                "{} bytes past a slot must not name it",
                offset
            );
        }
        // And nothing outside the region at either end.
        assert_eq!(slots.index_of(0x1_0000 - SLOT_BYTES), None);
        assert_eq!(slots.index_of(slots.end()), None);
        assert_eq!(slots.index_of(slots.end() - SLOT_BYTES), Some(3), "the last slot is inside");
    }

    /// The table fills, refuses, and frees.
    #[test]
    fn a_full_table_refuses_and_a_freed_slot_is_reused() {
        let mut slots: Slots<u32> = Slots::new(0x2_0000, 2);
        let a = slots.insert(1).expect("a free slot");
        let b = slots.insert(2).expect("a free slot");
        assert_ne!(a, b);
        assert_eq!(slots.live(), 2);
        assert_eq!(slots.insert(3), None, "a full table refuses rather than overwriting");

        assert_eq!(slots.remove(a), Some(1));
        assert_eq!(slots.live(), 1);
        assert_eq!(slots.remove(a), None, "removing twice finds nothing");
        assert_eq!(slots.insert(3), Some(a), "the lowest free slot is reused");
        assert_eq!(slots.get(a).copied(), Some(3));
        assert_eq!(slots.get(b).copied(), Some(2), "and the other entry is untouched");
    }

    /// `iter` yields every live entry with the address it lives at.
    #[test]
    fn iter_yields_every_live_entry_with_its_address() {
        let mut slots: Slots<u32> = Slots::new(0x3_0000, 4);
        for value in [1u32, 2, 3] {
            slots.insert(value).expect("a free slot");
        }
        let middle = slots.address_of(1);
        assert_eq!(slots.remove(middle), Some(2));
        let live: Vec<(usize, u32)> = slots.iter().map(|(at, value)| (at, *value)).collect();
        assert_eq!(live, vec![(0x3_0000, 1), (0x3_0000 + 2 * SLOT_BYTES, 3)]);
    }
}
