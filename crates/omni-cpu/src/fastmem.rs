//! The identity-mapping check (D4), and the 56-bit guest `PC`.
//!
//! # Why an assertion and not a comment
//!
//! D4 is the project's central bet: guest virtual address == host virtual address, so a guest
//! pointer *is* a host pointer and there is no translation on the memory path. It is not a design
//! aspiration but a **configuration**, and the configuration has a default that silently breaks it.
//!
//! Concretely, on the translating backend, `EmitFastmemVAddr` in dynarmic's
//! `backend/x64/emit_x64_memory.h` branches on `64 - fastmem_address_space_bits`:
//!
//! | `fastmem_address_space_bits` | What is emitted for a guest load |
//! |---|---|
//! | **64** | `r13 + vaddr` — nothing at all; the base folds into the SIB byte |
//! | anything less, mirroring | `mov`/`shl`/`shr` to mask the address into an arena, so a wild address silently **aliases a valid one** |
//! | anything less, not mirroring | `mov`/`shr`/`jnz` — and every address above `2^bits` leaves the fast path for a callback |
//!
//! dynarmic's own default is **36**. A guest address above 64 GiB then takes the callback path,
//! which the D4 spike measured at **396 Mguest-insn/s** against **5,207** on the fast path — a
//! **13.2x** loss that produces *correct results*. No functional test can see it. `libroblox.so`
//! loads high, so this is not a hypothetical.
//!
//! Hence [`require_identity_mapping`]: it is checked once per CPU context against what the backend
//! reports it is *actually* configured with, before any guest code runs, and a mismatch is a loud
//! typed failure rather than a slow runtime.
//!
//! # Why this type is backend-agnostic
//!
//! [`MemoryMapping`] is Omnidroid's vocabulary, not dynarmic's. `ARCHITECTURE.md` §6 has an
//! ARM64-host backend with no translator at all, where "identity mapping" is not a setting but a
//! tautology — it reports the identity mapping and passes the same check. A check written against
//! dynarmic's `UserConfig` would have had to be skipped there, and a check that is skipped is not a
//! check.

use crate::context::GuestAddressSpace;
use crate::error::{CpuError, CpuResult};

/// How many bits of guest `PC` the translating backend keeps, sign-extended (D4).
///
/// dynarmic's `A64::LocationDescriptor` packs the `PC` into 56 bits (`pc_bit_count = 56`), masking
/// on store and `sign_extend<56>`-ing on read, because the remaining bits of the 64-bit descriptor
/// carry `FPCR` and the single-stepping flag. So a guest `PC` is representable only if its top nine
/// bits (63 down to 55) are all equal.
///
/// Harmless for every address Omnidroid will really run: Windows user-space tops out at 128 TiB
/// (bit 47) and Android's at 512 GiB or 256 TiB depending on the page size, so bit 55 is never set
/// and the sign extension never fires. Recorded because it is a real cap, and because the failure it
/// would produce — a guest `PC` quietly turning into a different address — is one nobody would guess
/// at from the symptom.
pub const GUEST_PC_BITS: u32 = 56;

/// Whether a guest `PC` survives the [`GUEST_PC_BITS`] round trip unchanged.
///
/// True for every address below `2^55`, and for the sign-extended top of the address space. False
/// for the band in between, which no real mapping occupies.
#[must_use]
pub const fn pc_is_representable(pc: u64) -> bool {
    truncate_pc(pc) == pc
}

/// What the translating backend would store and read back for this `PC`.
#[must_use]
pub const fn truncate_pc(pc: u64) -> u64 {
    let shift = 64 - GUEST_PC_BITS;
    (((pc << shift) as i64) >> shift) as u64
}

/// What a backend reports its guest-memory path is *actually* configured with.
///
/// Read back from the live configuration, never echoed from what was asked for — the whole value of
/// the check is that it catches a setting being substituted underneath us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryMapping {
    /// Whether guest loads and stores reach memory directly rather than through host callbacks.
    pub direct_access: bool,
    /// Host address that guest address 0 maps to. **0** is identity mapping.
    pub host_base: u64,
    /// How many bits of guest address the direct path covers. **64** for a full-width space.
    pub address_bits: u64,
    /// Whether an address beyond `address_bits` is *masked* into range rather than faulting.
    ///
    /// At 64 bits this changes no emitted instruction, so it is not a performance setting — it is
    /// checked because if the width is ever narrowed, masking turns a wild guest address into a
    /// silent alias of a valid one instead of a fault, and Global Constraint 11 says wild guest
    /// addresses are the expected case.
    pub mirrors_out_of_range: bool,
    /// Whether a software page table is consulted on the memory path. Identity mapping means there
    /// is nothing to consult.
    pub page_table_present: bool,
    /// Whether the backend counts guest instructions.
    ///
    /// Checked here because the run loop's only working watchdog is a **short budget expiring**
    /// (see [`crate::run`]), and a budget cannot expire on a backend that is not counting.
    pub counts_instructions: bool,
    /// Host address of the `TPIDR_EL0` slot the backend reads through. Zero means guest code cannot
    /// read the thread pointer at all, which D13 says breaks the first stack-protected function in
    /// `libroblox.so`.
    pub tpidr_el0_slot: u64,
}

/// The configuration D4 requires, for a backend to compare itself against.
#[must_use]
pub const fn identity_mapping(tpidr_el0_slot: u64) -> MemoryMapping {
    MemoryMapping {
        direct_access: true,
        host_base: 0,
        address_bits: 64,
        mirrors_out_of_range: false,
        page_table_present: false,
        counts_instructions: true,
        tpidr_el0_slot,
    }
}

/// Refuse a CPU context whose memory path is not D4's identity mapping.
///
/// Called once per context before any guest code runs. Each failure names the setting, what it must
/// be, what it is, and — the part that matters — **what going ahead anyway would cost**, because
/// every one of these failures produces correct results and would otherwise be found by nobody.
///
/// # Errors
///
/// [`CpuError::MisconfiguredMemoryPath`] naming the first setting that is wrong.
pub fn require_identity_mapping(
    observed: &MemoryMapping,
    space: GuestAddressSpace,
) -> CpuResult<()> {
    let refuse = |setting, expected: u64, actual: u64, consequence| {
        Err(CpuError::MisconfiguredMemoryPath { setting, expected, actual, consequence })
    };

    if !observed.direct_access {
        return refuse(
            "direct guest memory access (fastmem)",
            1,
            0,
            "every guest load and store would go through a host callback: 396 against 5,207 \
             Mguest-insn/s in the D4 spike, a 13.2x loss, with correct results throughout",
        );
    }
    if observed.host_base != 0 {
        return refuse(
            "host base for guest address 0",
            0,
            observed.host_base,
            "a non-zero base means guest VA is not host VA, so D4 does not hold: every pointer the \
             loader hands to guest code, and every pointer guest code hands back, would need \
             translating",
        );
    }
    if observed.address_bits != 64 {
        return refuse(
            "guest address bits covered by the direct path",
            64,
            observed.address_bits,
            "below 64 the backend emits a mask or a bounds check on every access instead of \
             folding the base into the addressing mode, and every guest address above the limit \
             silently takes the 13.2x-slower callback path while still producing correct results. \
             dynarmic's own default here is 36",
        );
    }
    if observed.mirrors_out_of_range {
        return refuse(
            "mirroring of out-of-range guest addresses",
            0,
            1,
            "an address outside the mapped space would be masked into it and alias a valid page \
             instead of faulting, so a wild guest pointer would corrupt guest memory rather than \
             produce a typed exit (Global Constraint 11)",
        );
    }
    if observed.page_table_present {
        return refuse(
            "software page table on the memory path",
            0,
            1,
            "a page table is the alternative to identity mapping, not a supplement to it: it adds \
             a lookup to every guest access that D4 exists to remove",
        );
    }
    if !observed.counts_instructions {
        return refuse(
            "guest instruction counting",
            1,
            0,
            "the run loop's watchdog is a short step budget expiring, because Task 2 measured that \
             a direct-branch guest loop cannot be stopped by an external halt. A backend that does \
             not count cannot expire a budget, so untrusted guest code would have no bound at all",
        );
    }
    if observed.tpidr_el0_slot == 0 {
        return refuse(
            "TPIDR_EL0 storage",
            1,
            0,
            "guest code could not read the bionic thread pointer. libroblox.so holds 1,282 MRS \
             TPIDR_EL0 instructions, 1,276 of which load [Xt, #0x28], and the first runs before \
             JNI_OnLoad and before the first static initializer (D13)",
        );
    }
    if u64::from(space.address_bits()) > observed.address_bits {
        return refuse(
            "guest address bits covered by the direct path",
            u64::from(space.address_bits()),
            observed.address_bits,
            "the top of this guest address space is beyond the direct path's reach, so the highest \
             guest mappings would take the callback path",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> GuestAddressSpace {
        // D4 ran at host VA 0x7F00_0000_0000 (bit 46), which is where the loader puts a guest
        // library on Windows. 47 bits.
        GuestAddressSpace::new(0x7F00_0000_0000, 0x1_0000_0000).expect("a high guest space")
    }

    #[test]
    fn the_identity_configuration_is_accepted() {
        require_identity_mapping(&identity_mapping(0x1234), space()).expect("D4's configuration");
    }

    /// Every setting, refused one at a time, with the *reason* asserted and not just the variant.
    /// The reason is the deliverable here: all seven of these failures produce correct results, so
    /// what the message says is the only thing that will make anyone act on it.
    #[test]
    fn every_setting_that_breaks_d4_is_refused_by_name() {
        /// A named way of breaking the configuration, and the word its refusal must carry.
        type Case = (&'static str, fn(&mut MemoryMapping), &'static str);
        let cases: [Case; 6] = [
            ("fastmem off", |m| m.direct_access = false, "13.2x"),
            ("relocated base", |m| m.host_base = 0x1_0000, "guest VA is not host VA"),
            ("dynarmic's default width", |m| m.address_bits = 36, "default here is 36"),
            ("mirroring on", |m| m.mirrors_out_of_range = true, "alias a valid page"),
            ("page table present", |m| m.page_table_present = true, "adds a lookup"),
            ("no instruction counting", |m| m.counts_instructions = false, "no bound at all"),
        ];
        for (why, break_it, expected) in cases {
            let mut observed = identity_mapping(0x1234);
            break_it(&mut observed);
            let error = require_identity_mapping(&observed, space())
                .expect_err(&format!("{why} must be refused"));
            assert!(
                error.to_string().contains(expected),
                "{why} was refused for the wrong reason: {error}"
            );
            assert!(matches!(error, CpuError::MisconfiguredMemoryPath { .. }));
        }

        // D13's half, which is not about speed at all.
        let mut observed = identity_mapping(0);
        observed.tpidr_el0_slot = 0;
        let error = require_identity_mapping(&observed, space()).expect_err("no TPIDR_EL0");
        assert!(error.to_string().contains("1,276"), "{error}");
    }

    /// The narrowed-width case has to be caught even when it is wide enough for *some* space, or
    /// the check would pass on a small test space and fail in production.
    #[test]
    fn a_width_that_does_not_reach_the_top_of_the_space_is_refused() {
        // 47 bits of space against a 36-bit window: 0x7F00_0000_0000 is far above 2^36.
        let mut observed = identity_mapping(0x1234);
        observed.address_bits = 36;
        assert!(require_identity_mapping(&observed, space()).is_err());

        // And the same space with a low base still fails, because 64 is required outright — the
        // width check is a second line of defence, not the first.
        let low = GuestAddressSpace::new(0x1_0000, 0x10_0000).expect("a low space");
        assert!(low.address_bits() < 36);
        assert!(
            require_identity_mapping(&observed, low).is_err(),
            "a space that happens to fit a 36-bit window must still be refused: the guest can map \
             anywhere in its space later, and the assertion runs once"
        );
    }

    /// D4's second footgun, pinned. `truncate_pc` is what dynarmic's `LocationDescriptor` does:
    /// `pc & ones<56>` on store, `sign_extend<56>` on read.
    #[test]
    fn the_guest_pc_is_a_sign_extended_56_bit_value() {
        assert_eq!(GUEST_PC_BITS, 56);
        for pc in [
            0u64,
            4,
            0x7F00_0000_0000,          // where D4 ran: bit 46
            0x0000_7FFF_FFFF_FFFF,     // top of Windows user space, bit 46 upwards
            (1u64 << 55) - 4,          // last address below the sign bit
            u64::MAX & !3,             // sign-extends to itself
            0xFFFF_FF00_0000_0000,     // an all-ones top, which survives
        ] {
            assert!(pc_is_representable(pc), "{pc:#x} must survive the round trip");
        }
        // The band that does not survive: bit 55 set but the bits above it clear.
        let lost = 1u64 << 55;
        assert!(!pc_is_representable(lost));
        assert_eq!(truncate_pc(lost), 0xFF80_0000_0000_0000, "it sign-extends, it does not wrap");
        assert!(!pc_is_representable(0x00FF_FFFF_FFFF_FFFC));
    }
}
