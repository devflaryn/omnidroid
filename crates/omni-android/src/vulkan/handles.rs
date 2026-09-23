//! **The four handle families stage 3 issues, and the one rule they all obey.**
//!
//! # Why a guest Vulkan handle is an address in the boundary's data area
//!
//! [`instance::Instances`](super::instance) makes the argument for `VkInstance` and it is the same
//! argument four more times, so it is not repeated in full here — the short form is that
//! `VkInstance`, `VkPhysicalDevice`, `VkDevice` and `VkQueue` are **dispatchable** handles whose
//! first word is a loader dispatch table the driver dereferences, so a number the guest chose
//! reaching a driver is a host access violation from guest data (Global Constraint 11, Critical).
//! What the guest gets instead is an address in the boundary's own data area, and what comes back
//! out is a **token** the [`VulkanHost`](super::VulkanHost) implementation minted, so the driver's
//! pointer never enters this crate at all.
//!
//! `VkSurfaceKHR` is the interesting one, because it is **not** dispatchable: it is a 64-bit value
//! the driver looks up in its own table rather than dereferences, so passing the driver's value
//! through to the guest would not crash anything. It is behind a registry anyway, and the reason
//! is Global Constraint 1 rather than 11: a surface handle the guest invented would reach the
//! driver as a *plausible* surface, `vkGetPhysicalDeviceSurfaceSupportKHR` would answer about some
//! other surface, and the first thing to notice would be a swapchain presenting into a window
//! nobody chose. A wild handle must be a typed refusal in every family or it is a typed refusal in
//! none of them.
//!
//! # Why this is generic and [`Instances`](super::instance) is not folded into it
//!
//! Stage 2a's registry is shipping code with its own tests, and those tests are the ones that
//! establish the rule this file depends on — that a handle **off a slot boundary names nothing**,
//! because without the alignment test `(at - base) / SLOT_BYTES` turns every address in the region
//! into a valid index and a guest that computed `handle + 4` operates on the handle. Rewriting it
//! to be the fifth instantiation of this type would change working code to gain nothing a reader
//! can check. So the generic one is *new* code for the four *new* families, and the alignment test
//! is asserted again here, against this type, in this file's own tests.
//!
//! # What a slot holds, and what reads it back
//!
//! Sixteen bytes: a magic naming the family, and the slot's index. **Nothing reads either back.**
//! The identity is the address, [`Handles::index_of`] is what checks it, and the bytes exist so
//! that a guest that dereferences one of these handles — which is exactly what a driver would do,
//! and what this layer exists to prevent it from doing to a wild value — finds something
//! recognisable in a memory dump instead of a fault with no explanation.

use omni_mem::GuestAddr;

/// Bytes of the boundary's data area one issued handle occupies.
///
/// The same sixteen [`INSTANCE_SLOT_BYTES`](super::instance::INSTANCE_SLOT_BYTES) uses, and the
/// same sixteen [`ndk::SLOT_BYTES`](crate::ndk::SLOT_BYTES) uses, because the three are the same
/// idea and a reader looking at a dump should not have to work out which arena an address is in
/// before it can divide.
pub const HANDLE_SLOT_BYTES: usize = 16;

/// A registry of guest handles of one family, addressed by slot.
///
/// `T` is the host token — [`HostPhysicalDevice`](super::HostPhysicalDevice),
/// [`HostSurface`](super::HostSurface), [`HostDevice`](super::HostDevice) or
/// [`HostQueue`](super::HostQueue). It is deliberately **not** bounded by a trait: the only things
/// this type does with a token are store it, compare it and hand it back, and a trait bound naming
/// a method would suggest it does something else.
#[derive(Debug)]
pub(super) struct Handles<T> {
    base: GuestAddr,
    /// What this family is called, for a refusal that has to say which kind of handle was wrong.
    kind: &'static str,
    /// What a slot's first eight bytes say, for a reader of a dump. Nothing reads it back.
    magic: u64,
    entries: Vec<Option<T>>,
}

impl<T: Copy + PartialEq> Handles<T> {
    /// A registry of `capacity` slots starting at `base`.
    pub(super) fn new(base: GuestAddr, capacity: usize, kind: &'static str, magic: u64) -> Handles<T> {
        let mut entries = Vec::with_capacity(capacity);
        entries.resize_with(capacity, || None);
        Handles { base, kind, magic, entries }
    }

    /// What this family is called.
    pub(super) fn kind(&self) -> &'static str {
        self.kind
    }

    /// One past the last address this registry covers.
    pub(super) fn end(&self) -> GuestAddr {
        self.base + self.entries.len() * HANDLE_SLOT_BYTES
    }

    /// The guest address of slot `index`.
    pub(super) fn address_of(&self, index: usize) -> GuestAddr {
        self.base + index * HANDLE_SLOT_BYTES
    }

    /// How many slots this registry has at all.
    pub(super) fn capacity(&self) -> usize {
        self.entries.len()
    }

    /// The slot a guest handle names, **checked**: in range, and on a slot boundary.
    ///
    /// The alignment test is the whole point; this module's documentation says what its absence
    /// would cost.
    pub(super) fn index_of(&self, at: GuestAddr) -> Option<usize> {
        if at < self.base || at >= self.end() {
            return None;
        }
        let offset = at - self.base;
        (offset % HANDLE_SLOT_BYTES == 0).then_some(offset / HANDLE_SLOT_BYTES)
    }

    /// Put `token` in the lowest free slot and return its index and guest address.
    pub(super) fn insert(&mut self, token: T) -> Option<(usize, GuestAddr)> {
        let index = self.entries.iter().position(Option::is_none)?;
        self.entries[index] = Some(token);
        Some((index, self.address_of(index)))
    }

    /// The handle `token` already has, or a new one.
    ///
    /// **The deduplicating insert, and the three calls that need it need it badly.**
    /// `vkEnumeratePhysicalDevices` is called twice by every renderer — once for the count and
    /// once for the array — and the specification requires the second call to produce the same
    /// `VkPhysicalDevice` values as the first; `vkGetDeviceQueue` for one family and index must
    /// answer with the same `VkQueue` every time, because a renderer compares the graphics and
    /// present queue handles to decide whether its swapchain is `EXCLUSIVE` or `CONCURRENT`; and
    /// a second `vkEnumeratePhysicalDevices` on a *second* instance must **not** collide with the
    /// first, which is why the token and not the ordinal is what is compared.
    ///
    /// `Ok((index, address, fresh))`, where `fresh` says whether a slot was consumed — so a caller
    /// that wants to know whether it just issued a handle or recovered one can say so in a log.
    pub(super) fn insert_or_get(&mut self, token: T) -> Option<(usize, GuestAddr, bool)> {
        if let Some(index) = self.entries.iter().position(|slot| *slot == Some(token)) {
            return Some((index, self.address_of(index), false));
        }
        self.insert(token).map(|(index, at)| (index, at, true))
    }

    /// What a live guest handle names.
    pub(super) fn get(&self, at: GuestAddr) -> Option<T> {
        self.index_of(at).and_then(|index| self.entries[index])
    }

    /// Free the slot a live guest handle names, and answer what it held.
    ///
    /// # Why stage 4 needs this and stage 3 did not, and what it costs
    ///
    /// Stage 3 implemented no destructor at all — there is still no `vkDestroyInstance` in
    /// [`VulkanHost`](super::VulkanHost) — so every handle it issued stayed live and a registry
    /// that only grew was the honest shape. Stage 4 is the first one whose
    /// objects the guest genuinely destroys, once per frame in the case of a swapchain that
    /// follows a resize, so a registry with no removal would run a renderer out of
    /// [`MAX_SWAPCHAINS`](super::MAX_SWAPCHAINS) in a few seconds of dragging a window.
    /// `vkDestroySurfaceKHR` and `vkDestroyDevice` joined later, when the engine was measured
    /// calling them on the way out of `APP_CMD_TERM_WINDOW`; neither family deduplicates, so both
    /// meet the condition below.
    ///
    /// **Removal makes one thing unsound that was sound before, and it is named here rather than
    /// discovered later.** [`Handles::insert_or_get`] deduplicates on the token, and a freed slot
    /// can be refilled — so if a host ever reused a token for a *new* object, a stale guest handle
    /// would silently name the new one. The two registries that deduplicate are `VkQueue`, whose
    /// tokens are freed only by `vkDestroyDevice` and whose host never hands a destroyed device's
    /// queue tokens out again, and `VkImage`, whose tokens are freed only by
    /// `vkDestroySwapchainKHR` and whose host never reuses an index. A family that
    /// both deduplicates and has its tokens recycled must not use this method.
    ///
    /// The slot's sixteen bytes are deliberately **not** cleared: nothing reads them, this module
    /// says so, and a guest that dereferences a stale handle finding the family magic still there
    /// is more informative in a dump than one finding zeros.
    pub(super) fn remove(&mut self, at: GuestAddr) -> Option<T> {
        let index = self.index_of(at)?;
        self.entries[index].take()
    }

    /// Free every slot for which `keep` answers `false`, and answer how many were freed.
    ///
    /// What `vkDestroySwapchainKHR` uses to drop the `VkImage` handles that belonged to the
    /// swapchain it just destroyed: the guest never asked for those handles to go away and must
    /// not be able to use them afterwards, because a swapchain image's lifetime is its
    /// swapchain's.
    pub(super) fn retain(&mut self, mut keep: impl FnMut(T) -> bool) -> usize {
        let mut removed = 0;
        for slot in &mut self.entries {
            if slot.is_some_and(|token| !keep(token)) {
                *slot = None;
                removed += 1;
            }
        }
        removed
    }

    /// Every live handle, with the address the guest holds for it.
    pub(super) fn iter(&self) -> impl Iterator<Item = (GuestAddr, T)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.map(|token| (self.address_of(index), token)))
    }

    /// How many slots hold a handle.
    pub(super) fn live(&self) -> usize {
        self.entries.iter().filter(|slot| slot.is_some()).count()
    }

    /// The sixteen bytes slot `index` is filled with. See this module's documentation: nothing
    /// reads them back, and they are there for a reader of a dump.
    pub(super) fn slot_image(&self, index: usize) -> [u8; HANDLE_SLOT_BYTES] {
        let mut slot = [0u8; HANDLE_SLOT_BYTES];
        slot[..8].copy_from_slice(&self.magic.to_le_bytes());
        slot[8..].copy_from_slice(&(index as u64).to_le_bytes());
        slot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A handle off a slot boundary names nothing**, asserted against *this* type rather than
    /// inherited from `Instances`' test.
    ///
    /// Without the alignment test in [`Handles::index_of`], `(at - base) / 16` turns any address
    /// inside the region into a valid index, so a guest that computed `queue + 8` would operate on
    /// the queue. That is the one defect this file exists to make unreachable, so it is checked
    /// here and not assumed.
    #[test]
    fn a_handle_off_a_slot_boundary_names_nothing() {
        let mut handles: Handles<u64> = Handles::new(0x4_0000, 4, "VkQueue", 0x51);
        let (index, first) = handles.insert(7).expect("a free slot");
        assert_eq!(index, 0);
        assert_eq!(first, 0x4_0000);
        assert_eq!(handles.get(first), Some(7));
        for offset in 1..HANDLE_SLOT_BYTES {
            assert_eq!(
                handles.index_of(first + offset),
                None,
                "{offset} bytes past a handle must not name it"
            );
        }
        assert_eq!(handles.index_of(0x4_0000 - HANDLE_SLOT_BYTES), None, "below the base");
        assert_eq!(handles.index_of(handles.end()), None, "one past the end");
        assert_eq!(handles.get(handles.end() - HANDLE_SLOT_BYTES), None, "in range, unused");
    }

    /// **The same token gets the same handle back, and a different token does not.**
    ///
    /// The property `vkEnumeratePhysicalDevices`' two-call protocol and `vkGetDeviceQueue`'s
    /// idempotence both rest on. A registry that issued a fresh slot each time would hand the
    /// guest two `VkPhysicalDevice` values naming one device, and a renderer comparing its
    /// graphics and present queues would build a `CONCURRENT` swapchain for a device with one
    /// queue.
    #[test]
    fn the_same_token_gets_the_same_handle_and_consumes_one_slot() {
        let mut handles: Handles<u64> = Handles::new(0x5_0000, 4, "VkPhysicalDevice", 0x50);
        let (_, first, fresh) = handles.insert_or_get(11).expect("a free slot");
        assert!(fresh, "the first insert issues a handle");
        let (_, again, fresh) = handles.insert_or_get(11).expect("the same slot");
        assert_eq!(again, first, "one token, one handle");
        assert!(!fresh, "and the second call consumed no slot");
        assert_eq!(handles.live(), 1);

        let (_, other, fresh) = handles.insert_or_get(12).expect("a free slot");
        assert!(fresh);
        assert_ne!(other, first, "a different token is a different handle");
        assert_eq!(handles.live(), 2);
    }

    /// **A full registry refuses rather than reusing a live slot**, and says how many it has.
    #[test]
    fn a_full_registry_refuses_rather_than_reusing_a_live_slot() {
        let mut handles: Handles<u64> = Handles::new(0x6_0000, 2, "VkDevice", 0x44);
        assert_eq!(handles.capacity(), 2);
        assert!(handles.insert(1).is_some());
        assert!(handles.insert(2).is_some());
        assert_eq!(handles.insert(3), None, "the third is refused, not written over the first");
        assert_eq!(handles.live(), 2);
        let live: Vec<(GuestAddr, u64)> = handles.iter().collect();
        assert_eq!(live, vec![(0x6_0000, 1), (0x6_0010, 2)]);
        assert_eq!(handles.kind(), "VkDevice");
    }

    /// **A removed handle names nothing, and its slot is reused rather than leaked.**
    ///
    /// The property `vkDestroySwapchainKHR` rests on, and the one a registry that only grew could
    /// not have: a renderer that recreates its swapchain once per resize destroys and creates
    /// dozens of them, and a `MAX_SWAPCHAINS` that counted every one ever made would refuse a
    /// conforming program after a second of dragging a window.
    #[test]
    fn a_removed_handle_names_nothing_and_frees_its_slot() {
        let mut handles: Handles<u64> = Handles::new(0x8_0000, 2, "VkSwapchainKHR", 0x53);
        let (_, first) = handles.insert(1).expect("a free slot");
        let (_, second) = handles.insert(2).expect("a free slot");
        assert_eq!(handles.insert(3), None, "full");

        assert_eq!(handles.remove(first), Some(1));
        assert_eq!(handles.get(first), None, "the handle names nothing now");
        assert_eq!(handles.remove(first), None, "and a second destroy finds nothing");
        assert_eq!(handles.live(), 1);

        let (_, again) = handles.insert(3).expect("the freed slot");
        assert_eq!(again, first, "the slot was reused rather than leaked");
        assert_eq!(handles.get(second), Some(2), "and the other handle is untouched");
    }

    /// **`retain` frees exactly the slots it is told to**, which is how a destroyed swapchain
    /// takes its images' handles with it.
    #[test]
    fn retain_frees_the_handles_that_belonged_to_something_that_is_gone() {
        let mut handles: Handles<u64> = Handles::new(0x9_0000, 4, "VkImage", 0x49);
        for token in [10, 11, 20, 21] {
            handles.insert(token).expect("a free slot");
        }
        // The images of "swapchain 1" are 10 and 11; "swapchain 2" owns 20 and 21.
        let removed = handles.retain(|token| token >= 20);
        assert_eq!(removed, 2);
        assert_eq!(handles.live(), 2);
        let left: Vec<u64> = handles.iter().map(|(_, token)| token).collect();
        assert_eq!(left, vec![20, 21]);
    }

    /// **A slot's bytes name the family and the index**, so a dump is readable.
    #[test]
    fn a_slot_image_carries_the_family_magic_and_the_index() {
        let handles: Handles<u64> = Handles::new(0x7_0000, 2, "VkSurfaceKHR", 0x0053_5255_4656_4B00);
        let image = handles.slot_image(1);
        assert_eq!(u64::from_le_bytes(image[..8].try_into().expect("eight")), 0x0053_5255_4656_4B00);
        assert_eq!(u64::from_le_bytes(image[8..].try_into().expect("eight")), 1);
    }
}
