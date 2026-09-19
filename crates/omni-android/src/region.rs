//! The reserved thunk region: guest addresses that are not guest code.
//!
//! # A correction to the plan, and to the M3 brief
//!
//! Both say the loader "already enumerates imports (M1) **and binds them to synthetic guest addresses
//! in a reserved thunk region**". The first half is true. The second is not: `omni-elf` has a
//! [`SymbolProvider`](omni_elf::loader::SymbolProvider) trait and an `EmptyProvider` that resolves
//! nothing, and the only provider in the workspace before this task was that one. There was no
//! region, no address assignment and no allocator anywhere. So reserving the region is part of this
//! task rather than something it consumes, and `ARCHITECTURE.md` section 5's description of the
//! region is a design statement rather than a description of code.
//!
//! # Two areas, because functions and data are not the same problem
//!
//! Of the 188 reachable imports, **18 are `STT_OBJECT` data** — `environ`, `stdin`/`stdout`/`stderr`,
//! `__sF`, `__stack_chk_guard`, the ten `AMEDIAFORMAT_KEY_*` strings, `in6addr_any`. The guest
//! *loads* from those, so they need real mapped, readable guest memory holding real bytes. The other
//! 170 are called, so they need an address that is not memory at all.
//!
//! | Area | Protection | Why |
//! |---|---|---|
//! | **functions** | `Read` — deliberately **not** executable | see below |
//! | **data** | `ReadWrite` | the guest loads from it, and `environ` and `errno` are written |
//!
//! # Why the function area is not executable
//!
//! Because of the hostile case the brief names: a guest that branches into the **middle** of a thunk
//! rather than to its start. The translating backend's instruction fetch answers for a *registered*
//! address before it ever looks at guest memory, so a slot's first word needs no contents. Any other
//! address in the area then reaches the fetch, which asks for `ReadExecute`, is refused on protection,
//! and the guest gets a typed [`ExitReason::MemoryFault`](omni_cpu::ExitReason::MemoryFault) naming
//! the address. The alternative — an executable area filled with something — would execute whatever
//! that something is, four bytes into it.
//!
//! The area is also [`CommitPolicy::Lazy`], so it costs no commit charge at all (Global Constraint 6):
//! nothing ever reads it on the path that works, and the protection check happens before `admit`'s
//! commit rule, so not even a hostile fetch commits a page.
//!
//! # Why a slot is 16 bytes and not 4
//!
//! **This is where the ARM64-native path nearly got foreclosed.** On the translating backend a thunk
//! is an address the backend recognises, so one word — or none — would do, and 170 slots of 4 bytes is
//! a tidy 680 bytes. On an ARM64 host there is no translator to recognise anything: the backend plants
//! a **veneer** in guest memory and the guest's `BL` really does execute it
//! (`ARCHITECTURE.md` section 6, D5). The smallest veneer that can reach an arbitrary 64-bit host
//! address is four instructions —
//!
//! ```text
//!     LDR  X16, #8      ; the literal below
//!     BR   X16
//!     .quad host_entry
//! ```
//!
//! — which is 16 bytes. A 4-byte slot would have made the region's layout, and therefore every
//! address the loader had already written into 568,806 relocated `GOT` slots, unable to hold the
//! ARM64 form. It is not a cost anybody pays: 170 slots at 16 bytes is 2,720 bytes of address space,
//! lazily committed, in a region that is never read.

use std::sync::Arc;

use omni_mem::{CommitPolicy, GuestAddr, GuestSpace, Placement, Protection};

use crate::error::{AbiError, AbiResult};

/// Bytes per function slot. See the module docs: four instructions, so the ARM64-native veneer fits.
pub const SLOT_BYTES: usize = 16;

/// The smallest veneer that can reach an arbitrary 64-bit host address, in instructions.
///
/// Stated as a constant so that [`SLOT_BYTES`] is visibly derived from it rather than chosen.
pub const VENEER_INSTRUCTIONS: usize = 4;

/// A reserved range of guest address space that holds no guest code.
#[derive(Debug)]
pub struct ThunkRegion {
    space: Arc<GuestSpace>,
    functions: GuestAddr,
    function_bytes: usize,
    next_slot: usize,
    data: GuestAddr,
    data_bytes: usize,
    next_data: usize,
}

impl ThunkRegion {
    /// Reserve a region with room for `slots` functions and `data_bytes` of data objects.
    ///
    /// # Errors
    ///
    /// [`AbiError::Memory`] if the guest address space could not supply either area.
    pub fn reserve(space: Arc<GuestSpace>, slots: usize, data_bytes: usize) -> AbiResult<Self> {
        let page = space.page_size();
        let function_bytes = round_up(slots.max(1) * SLOT_BYTES, page);
        let data_bytes = round_up(data_bytes.max(1), page);

        let functions = space.map_anonymous(
            Placement::Anywhere { align: page },
            function_bytes,
            // Not executable, and lazily committed. See the module docs: this is what turns a branch
            // into the middle of a slot into a typed fault instead of four bytes of something.
            Protection::Read,
            CommitPolicy::Lazy,
        )?;
        let data = space.map_anonymous(
            Placement::Anywhere { align: page },
            data_bytes,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )?;
        Ok(Self {
            space,
            functions,
            function_bytes,
            next_slot: 0,
            data,
            data_bytes,
            next_data: 0,
        })
    }

    /// First address of the function area.
    #[must_use]
    pub fn functions_start(&self) -> GuestAddr {
        self.functions
    }

    /// One past the last address of the function area.
    #[must_use]
    pub fn functions_end(&self) -> GuestAddr {
        self.functions + self.function_bytes
    }

    /// First address of the data area.
    #[must_use]
    pub fn data_start(&self) -> GuestAddr {
        self.data
    }

    /// One past the last address of the data area.
    #[must_use]
    pub fn data_end(&self) -> GuestAddr {
        self.data + self.data_bytes
    }

    /// How many function slots have been handed out.
    #[must_use]
    pub fn slots_used(&self) -> usize {
        self.next_slot
    }

    /// How many more there is room for.
    #[must_use]
    pub fn slots_free(&self) -> usize {
        self.function_bytes / SLOT_BYTES - self.next_slot
    }

    /// Take the next function slot.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`], naming the numbers — because a region that ran out silently would
    /// bind the remaining imports to whatever came next in the address space.
    pub fn allocate_function(&mut self) -> AbiResult<GuestAddr> {
        if self.slots_free() == 0 {
            return Err(AbiError::RegionFull {
                what: "function slot",
                detail: format!(
                    "the function area is {} bytes, which is {} slots of {SLOT_BYTES}, and all of \
                     them are taken",
                    self.function_bytes,
                    self.function_bytes / SLOT_BYTES
                ),
            });
        }
        let address = self.functions + self.next_slot * SLOT_BYTES;
        self.next_slot += 1;
        Ok(address)
    }

    /// Take `len` bytes of the data area, aligned to `align`.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`].
    pub fn allocate_data(&mut self, len: usize, align: usize) -> AbiResult<GuestAddr> {
        let align = align.max(1);
        let start = round_up(self.data + self.next_data, align) - self.data;
        if len > self.data_bytes.saturating_sub(start) {
            return Err(AbiError::RegionFull {
                what: "data object",
                detail: format!(
                    "the data area is {} bytes, {start} are used, and {len} more were asked for \
                     at alignment {align}",
                    self.data_bytes
                ),
            });
        }
        self.next_data = start + len;
        Ok(self.data + start)
    }

    /// Whether `address` is anywhere in the function area.
    #[must_use]
    pub fn holds_function(&self, address: GuestAddr) -> bool {
        (self.functions..self.functions_end()).contains(&address)
    }

    /// Whether `address` is anywhere in the data area.
    #[must_use]
    pub fn holds_data(&self, address: GuestAddr) -> bool {
        (self.data..self.data_end()).contains(&address)
    }

    /// The slot `address` falls in, and how far into it, for an address in the function area.
    ///
    /// The second element is what distinguishes a call from a branch into the middle of a slot, which
    /// is the difference between servicing a symbol and reporting [`AbiError::MidThunk`].
    #[must_use]
    pub fn slot_of(&self, address: GuestAddr) -> Option<(GuestAddr, usize)> {
        if !self.holds_function(address) {
            return None;
        }
        let offset = (address - self.functions) % SLOT_BYTES;
        Some((address - offset, offset))
    }

    /// The address space this region lives in.
    #[must_use]
    pub fn space(&self) -> &Arc<GuestSpace> {
        &self.space
    }
}

fn round_up(value: usize, to: usize) -> usize {
    debug_assert!(to.is_power_of_two() || to == 1);
    (value + to - 1) & !(to - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_mem::FaultAccess;

    fn region(slots: usize) -> ThunkRegion {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        ThunkRegion::reserve(space, slots, 1024).expect("a thunk region")
    }

    /// The whole reason the slot is 16 bytes rather than 4: the ARM64-native veneer has to fit, and
    /// the slot size is baked into every address the loader writes into a relocated `GOT` slot.
    #[test]
    fn a_slot_is_large_enough_for_the_arm64_native_veneer() {
        assert_eq!(VENEER_INSTRUCTIONS, 4);
        assert_eq!(SLOT_BYTES, VENEER_INSTRUCTIONS * 4);
        // `LDR X16, #8` + `BR X16` + an 8-byte literal is two instructions and two words of data,
        // which is the same 16 bytes counted the other way.
        assert_eq!(SLOT_BYTES, 2 * 4 + 8);
    }

    #[test]
    fn slots_are_handed_out_in_order_and_do_not_overlap() {
        let mut r = region(170);
        let first = r.allocate_function().expect("a slot");
        let second = r.allocate_function().expect("a slot");
        assert_eq!(first, r.functions_start());
        assert_eq!(second - first, SLOT_BYTES);
        assert_eq!(r.slots_used(), 2);
        assert!(r.holds_function(first) && r.holds_function(second));
        assert!(!r.holds_data(first), "the two areas are separate mappings");
    }

    /// The region running out must name the numbers rather than bind the next import to whatever
    /// follows the mapping.
    #[test]
    fn a_full_function_area_refuses_by_name_rather_than_running_on() {
        let space = Arc::new(GuestSpace::new().expect("space"));
        let mut r = ThunkRegion::reserve(Arc::clone(&space), 1, 16).expect("region");
        let capacity = r.slots_free();
        for _ in 0..capacity {
            r.allocate_function().expect("within capacity");
        }
        let error = r.allocate_function().expect_err("past capacity");
        match error {
            AbiError::RegionFull { what, detail } => {
                assert_eq!(what, "function slot");
                assert!(detail.contains(&capacity.to_string()), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn data_objects_are_aligned_as_asked_and_stay_inside_the_area() {
        let mut r = region(8);
        let a = r.allocate_data(1, 1).expect("a byte");
        let b = r.allocate_data(8, 8).expect("a pointer");
        assert_eq!(b % 8, 0, "alignment is honoured even after an odd allocation");
        assert!(b > a);
        assert!(r.holds_data(a) && r.holds_data(b));
        assert!(r.data_end() > b);
        let error = r.allocate_data(usize::MAX / 2, 8).expect_err("absurd size refused");
        assert!(matches!(error, AbiError::RegionFull { what: "data object", .. }), "{error:?}");
    }

    /// **The hostile case, at the level the region owns it.** An address four bytes into a slot must
    /// be identifiable as such, so that the boundary can report it rather than service the symbol.
    #[test]
    fn an_address_inside_a_slot_reports_the_slot_and_the_offset() {
        let mut r = region(4);
        let slot = r.allocate_function().expect("a slot");
        assert_eq!(r.slot_of(slot), Some((slot, 0)), "the start of the slot has offset zero");
        for offset in 1..SLOT_BYTES {
            assert_eq!(
                r.slot_of(slot + offset),
                Some((slot, offset)),
                "an address inside a slot must resolve to that slot and its offset"
            );
        }
        assert_eq!(r.slot_of(slot + SLOT_BYTES), Some((slot + SLOT_BYTES, 0)), "the next slot");
        assert_eq!(r.slot_of(r.functions_end()), None, "one past the end is outside");
        assert_eq!(r.slot_of(r.data_start()), None, "the data area is not the function area");
    }

    /// The claim the mid-thunk defence rests on: the function area is **not** executable, so any fetch
    /// from it that the backend does not answer for itself is refused on protection.
    #[test]
    fn the_function_area_is_not_executable_so_a_fetch_from_it_is_refused() {
        let r = region(4);
        let space = r.space();
        let at = r.functions_start() + 4;
        let refusal = omni_mem::admit(space, at, 4, FaultAccess::Execute)
            .expect_err("an instruction fetch from the function area must be refused");
        assert_eq!(
            refusal,
            omni_mem::Refusal::Protection,
            "refused on protection, which happens before the commit rule — so not even a hostile \
             fetch commits a page"
        );
        // Reading is allowed, which is what lets a diagnostic dump the area.
        omni_mem::admit(space, at, 4, FaultAccess::Read).expect("reading is allowed");
    }

    /// Global Constraint 6: the function area is never read on the path that works, so it must cost
    /// nothing. Asserted through the region list rather than through a process counter, which is a
    /// process-wide quantity two tests would measure of each other.
    #[test]
    fn the_function_area_costs_no_commit_charge() {
        let r = region(170);
        let info = r
            .space()
            .region_at(r.functions_start())
            .expect("the function area is mapped");
        assert_eq!(info.committed, 0, "lazily committed, and nothing has touched it");
        let data = r.space().region_at(r.data_start()).expect("the data area is mapped");
        assert!(data.committed > 0, "the data area is eager: the guest loads from it");
    }
}
