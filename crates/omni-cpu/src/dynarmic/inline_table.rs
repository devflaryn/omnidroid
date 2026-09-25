//! The inline thunks of one context, found by address in O(1) on the `SVC` path.
//!
//! # Why not the `BTreeMap` alone
//!
//! `cb_call_svc` looks the site up on **every** in-loop import crossing -- five to seven million a
//! second in a world -- and a `BTreeMap` of the ~600 imports a loaded `libroblox.so` has is a walk
//! of several nodes to do it. MEASURED (sampling profiler, in-world, 2026-09-25): `cb_call_svc` was
//! 4.7% of the samples outside translated code, most of it that walk. `omni-android`'s boundary
//! already moved its own slot lookup to an array index for the same reason (`SlotTable`).
//!
//! # The shape, without knowing the region's layout
//!
//! This crate does not know where the embedding puts its thunks, or how far apart. What it can see
//! is the set it was given: the embedding plants them in one dense region at a fixed stride
//! (`omni-android`'s `ThunkRegion`, sixteen-byte slots). So the table is a **window** starting at
//! the lowest registered address, indexed by `(site - base) >> shift`, where `1 << shift` is the
//! largest power of two dividing every registered offset from that base -- the region's slot size,
//! derived rather than assumed. An entry holds the position of the thunk in `list`, and the lookup
//! checks the address it finds against the site before answering.
//!
//! **What stays on the map.** Anything the window cannot hold -- a thunk beyond
//! [`WINDOW_SLOTS`] strides from the base, a site not on the stride -- is answered by the map,
//! so the window is a cache of the map that can only miss, never disagree. A site inside the
//! window, on the stride, with an empty entry is **not** a thunk, and is answered as such without
//! the map, because every registered address inside the window has an entry.
//!
//! **Rebuilt lazily.** `insert` and `remove` mark the window stale and the next lookup rebuilds it
//! once. The ~600 insertions a boundary makes at install would otherwise rebuild it ~600 times.

use std::collections::BTreeMap;

use omni_mem::GuestAddr;

use crate::thunk::{ThunkContext, ThunkFn};

/// The most strides the window spans. Sixteen thousand sixteen-byte slots is a 256 KiB thunk
/// region, several times what `libroblox.so`'s imports and the JNI tables need, and it costs 64
/// KiB of `u32`s per context at the most; a window over a real region is a few KiB.
pub(crate) const WINDOW_SLOTS: usize = 16 * 1024;

/// No thunk at this window position.
const EMPTY: u32 = u32::MAX;

/// One inline thunk: its address, and what services it.
type Entry = (GuestAddr, ThunkFn, ThunkContext);

#[derive(Default)]
pub(crate) struct InlineThunks {
    /// The authority: every inline thunk, by address.
    map: BTreeMap<GuestAddr, (ThunkFn, ThunkContext)>,
    /// Every thunk the window holds, in address order. The window's entries index this.
    list: Vec<Entry>,
    /// Window position `n` (at `base + (n << shift)`) to a position in `list`, or [`EMPTY`].
    window: Box<[u32]>,
    base: GuestAddr,
    shift: u32,
    /// `insert` or `remove` ran since the window was last built.
    stale: bool,
}

impl InlineThunks {
    pub(crate) fn insert(&mut self, address: GuestAddr, handler: ThunkFn, context: ThunkContext) {
        self.map.insert(address, (handler, context));
        self.stale = true;
    }

    pub(crate) fn remove(&mut self, address: &GuestAddr) -> bool {
        let had = self.map.remove(address).is_some();
        self.stale = true;
        had
    }

    /// Whether `address` is an inline thunk. The translation path's question, answered by the map:
    /// it runs once per translated instruction, not once per crossing.
    pub(crate) fn contains_key(&self, address: &GuestAddr) -> bool {
        self.map.contains_key(address)
    }

    /// The handler and context registered at exactly `site`, if any. O(1) for a site in the window.
    #[inline]
    pub(crate) fn get(&mut self, site: GuestAddr) -> Option<(ThunkFn, ThunkContext)> {
        if self.stale {
            self.rebuild();
        }
        let offset = site.wrapping_sub(self.base);
        if offset & ((1 << self.shift) - 1) == 0 {
            if let Some(&position) = self.window.get(offset >> self.shift) {
                if position == EMPTY {
                    return None;
                }
                if let Some(&(address, handler, context)) = self.list.get(position as usize) {
                    if address == site {
                        return Some((handler, context));
                    }
                }
            }
        }
        self.map.get(&site).copied()
    }

    /// Lay the window over the map as it now is.
    #[cold]
    fn rebuild(&mut self) {
        self.stale = false;
        self.list.clear();
        let Some(&base) = self.map.keys().next() else {
            self.window = Box::new([]);
            return;
        };
        // The largest power of two that divides every offset from the base: the stride the
        // thunks were planted at. Zero offsets (the base itself) divide by anything.
        let common = self.map.keys().fold(0usize, |bits, &address| bits | (address - base));
        let shift = if common == 0 { 0 } else { common.trailing_zeros() };
        let last = self.map.keys().next_back().copied().unwrap_or(base);
        let slots = ((last - base) >> shift).saturating_add(1).min(WINDOW_SLOTS);
        let mut window = vec![EMPTY; slots].into_boxed_slice();
        for (&address, &(handler, context)) in &self.map {
            let n = (address - base) >> shift;
            if n >= slots {
                break;
            }
            window[n] = u32::try_from(self.list.len()).expect("the window holds fewer than 2^32 thunks");
            self.list.push((address, handler, context));
        }
        self.window = window;
        self.base = base;
        self.shift = shift;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thunk::ThunkCall;

    fn one(_call: &mut ThunkCall<'_>) {}
    fn two(_call: &mut ThunkCall<'_>) {}

    fn context_at(table: &mut InlineThunks, site: GuestAddr) -> Option<usize> {
        table.get(site).map(|(_, context)| context.0)
    }

    #[test]
    fn every_registered_address_and_nothing_else_is_found() {
        let mut table = InlineThunks::default();
        let base: GuestAddr = 0x7000_0000;
        for n in 0..600 {
            table.insert(base + n * 16, if n % 2 == 0 { one } else { two }, ThunkContext(n));
        }
        for n in 0..600 {
            assert_eq!(context_at(&mut table, base + n * 16), Some(n), "slot {n}");
            for inside in [4, 8, 12, 1, 15] {
                assert_eq!(context_at(&mut table, base + n * 16 + inside), None, "slot {n} + {inside}");
            }
        }
        assert_eq!(table.shift, 4, "the stride is derived from the addresses: sixteen bytes");
        for outside in [base - 16, base - 4, base + 600 * 16, 0, GuestAddr::MAX] {
            assert_eq!(context_at(&mut table, outside), None, "{outside:#x}");
        }
    }

    #[test]
    fn a_thunk_beyond_the_window_is_still_found_through_the_map() {
        let mut table = InlineThunks::default();
        let base: GuestAddr = 0x1000;
        table.insert(base, one, ThunkContext(1));
        table.insert(base + 16, one, ThunkContext(2));
        let far = base + 16 * (WINDOW_SLOTS + 100);
        table.insert(far, two, ThunkContext(3));
        assert_eq!(context_at(&mut table, base), Some(1));
        assert_eq!(context_at(&mut table, base + 16), Some(2));
        assert_eq!(context_at(&mut table, far), Some(3), "beyond the window: the map answers");
        assert_eq!(table.window.len(), WINDOW_SLOTS, "and the window stopped at its bound");
    }

    #[test]
    fn a_removed_thunk_is_gone_and_a_lower_one_moves_the_window() {
        let mut table = InlineThunks::default();
        table.insert(0x2000, one, ThunkContext(1));
        table.insert(0x2010, one, ThunkContext(2));
        assert_eq!(context_at(&mut table, 0x2010), Some(2));
        assert!(table.remove(&0x2010));
        assert_eq!(context_at(&mut table, 0x2010), None, "removed");
        table.insert(0x1ff0, two, ThunkContext(3));
        assert_eq!(context_at(&mut table, 0x1ff0), Some(3), "below the old base");
        assert_eq!(context_at(&mut table, 0x2000), Some(1));
        assert!(!table.remove(&0x3000));
        assert_eq!(InlineThunks::default().get(0x2000).map(|(_, c)| c.0), None, "empty");
    }

    #[test]
    fn thunks_off_a_common_stride_are_all_found() {
        let mut table = InlineThunks::default();
        let sites = [0x4000, 0x4004, 0x4010, 0x4024];
        for (n, &site) in sites.iter().enumerate() {
            table.insert(site, one, ThunkContext(n));
        }
        for (n, &site) in sites.iter().enumerate() {
            assert_eq!(context_at(&mut table, site), Some(n), "{site:#x}");
        }
        assert_eq!(context_at(&mut table, 0x4008), None);
        assert_eq!(table.shift, 2);
    }
}
