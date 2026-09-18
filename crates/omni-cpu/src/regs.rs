//! The guest register file, as names rather than as indices.
//!
//! Every type here is a validated value. An index that does not name a register cannot be built, so
//! no backend has to decide what to do with one — which matters because the indices will ultimately
//! come out of guest instruction encodings and out of a debugger, and neither is trusted input
//! (Global Constraint 11).

use crate::error::{CpuError, CpuResult};

/// One of the 31 general-purpose registers, `X0` to `X30`.
///
/// # Why `X31` is not here
///
/// AArch64 has no `X31`. Encoding 31 means the zero register `XZR`/`WZR` in most instructions and
/// the stack pointer `SP` in a few, and which one it means is decided by the *instruction*, not by
/// the register field. A register file that offered `X31` would therefore have to invent an answer.
/// So [`SP`](crate::GuestCpu::sp) is its own accessor, `XZR` is not addressable at all, and the
/// range here is exactly 0..=30.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct XReg(u8);

impl XReg {
    /// How many general-purpose registers there are: 31, `X0` through `X30`.
    pub const COUNT: usize = 31;

    /// The link register, `X30`. Named because the return address is the one register the runtime
    /// itself sets, when it plants a sentinel to detect a guest function returning.
    pub const LR: XReg = XReg(30);

    /// The frame pointer, `X29`. Named for the same reason: a stack walker needs it.
    pub const FP: XReg = XReg(29);

    /// The first argument and result register, `X0`, under the AAPCS64 procedure call standard.
    pub const X0: XReg = XReg(0);

    /// Name a register by index.
    ///
    /// # Errors
    ///
    /// [`CpuError::NoSuchRegister`] for 31 or above — see the type's documentation for why 31 in
    /// particular is refused rather than mapped to `XZR` or `SP`.
    pub const fn new(index: u8) -> CpuResult<Self> {
        if index as usize >= Self::COUNT {
            return Err(CpuError::NoSuchRegister { class: "X", index: index as u32, count: 31 });
        }
        Ok(Self(index))
    }

    /// The register's index, 0..=30.
    #[must_use]
    pub const fn index(self) -> u8 {
        self.0
    }

    /// Every general-purpose register in order, so a caller can round-trip the whole file.
    pub fn all() -> impl Iterator<Item = XReg> {
        (0..Self::COUNT as u8).map(XReg)
    }
}

impl core::fmt::Display for XReg {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "x{}", self.0)
    }
}

/// One of the 32 SIMD and floating-point registers, `V0` to `V31`.
///
/// 128 bits wide. `Q`, `D`, `S`, `H` and `B` views are the same register read at different widths,
/// so there is one accessor and the caller narrows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VReg(u8);

impl VReg {
    /// How many vector registers there are: 32.
    pub const COUNT: usize = 32;

    /// `V0`, the first argument and result register for floating-point and vector values.
    pub const V0: VReg = VReg(0);

    /// Name a vector register by index.
    ///
    /// # Errors
    ///
    /// [`CpuError::NoSuchRegister`] for 32 or above.
    pub const fn new(index: u8) -> CpuResult<Self> {
        if index as usize >= Self::COUNT {
            return Err(CpuError::NoSuchRegister { class: "V", index: index as u32, count: 32 });
        }
        Ok(Self(index))
    }

    /// The register's index, 0..=31.
    #[must_use]
    pub const fn index(self) -> u8 {
        self.0
    }

    /// Every vector register in order.
    pub fn all() -> impl Iterator<Item = VReg> {
        (0..Self::COUNT as u8).map(VReg)
    }
}

impl core::fmt::Display for VReg {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// The four condition flags: negative, zero, carry, overflow.
///
/// Four `bool`s rather than a bit mask, because the mask is where the mistakes live. `NZCV` occupies
/// bits 31..28 of `PSTATE`, and the rest of that word holds `PSTATE` bits the guest must not be able
/// to set from a register write — `PAN`, `SSBS`, `DIT` and the execution state bits among them. A
/// `u32` accessor would carry all of them; this type cannot, and
/// [`from_pstate`](Nzcv::from_pstate) is the one place the masking happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Nzcv {
    /// Negative: bit 31 of `PSTATE`.
    pub n: bool,
    /// Zero: bit 30.
    pub z: bool,
    /// Carry: bit 29.
    pub c: bool,
    /// Overflow: bit 28.
    pub v: bool,
}

impl Nzcv {
    /// The bits of `PSTATE` that this type covers, and the only ones a register write may set.
    pub const MASK: u32 = 0xF000_0000;

    /// Read the four flags out of a `PSTATE` word, **discarding every other bit**.
    ///
    /// Lossy on purpose. The alternative — refusing a word with other bits set — would make a
    /// perfectly ordinary `MRS Xt, NZCV` result unusable, and the alternative to *that* — keeping
    /// the other bits — would let a guest register write reach `PSTATE` bits it has no business
    /// setting.
    #[must_use]
    pub const fn from_pstate(pstate: u64) -> Self {
        let bits = (pstate as u32) & Self::MASK;
        Self {
            n: bits & (1 << 31) != 0,
            z: bits & (1 << 30) != 0,
            c: bits & (1 << 29) != 0,
            v: bits & (1 << 28) != 0,
        }
    }

    /// The four flags as a `PSTATE` word, with every other bit zero.
    #[must_use]
    pub const fn to_pstate(self) -> u64 {
        ((self.n as u32) << 31 | (self.z as u32) << 30 | (self.c as u32) << 29 | (self.v as u32)
            << 28) as u64
    }
}

impl core::fmt::Display for Nzcv {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let flag = |set, name| if set { name } else { '-' };
        write!(
            f,
            "{}{}{}{}",
            flag(self.n, 'N'),
            flag(self.z, 'Z'),
            flag(self.c, 'C'),
            flag(self.v, 'V')
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_register_index_that_names_no_register_is_refused() {
        assert_eq!(XReg::new(0).expect("x0").index(), 0);
        assert_eq!(XReg::new(30).expect("x30").index(), 30);
        for index in [31u8, 32, 63, 64, 128, 255] {
            match XReg::new(index) {
                Err(CpuError::NoSuchRegister { class, index: reported, count }) => {
                    assert_eq!(class, "X");
                    assert_eq!(reported, u32::from(index));
                    assert_eq!(count, 31);
                }
                other => panic!("x{index} must be refused, got {other:?}"),
            }
        }

        assert_eq!(VReg::new(31).expect("v31").index(), 31);
        for index in [32u8, 33, 255] {
            assert!(matches!(VReg::new(index), Err(CpuError::NoSuchRegister { class: "V", .. })));
        }

        assert_eq!(XReg::all().count(), 31);
        assert_eq!(VReg::all().count(), 32);
    }

    /// A `PSTATE` word carries far more than the condition flags, and none of the rest may survive
    /// a round trip through a register write.
    #[test]
    fn only_the_four_condition_flags_survive_a_pstate_round_trip() {
        assert_eq!(Nzcv::from_pstate(0).to_pstate(), 0);
        assert_eq!(Nzcv::from_pstate(0xF000_0000).to_pstate(), 0xF000_0000);

        // Every other bit of the low word, plus the whole high word, is dropped.
        let hostile = 0xFFFF_FFFF_0FFF_FFFFu64;
        assert_eq!(Nzcv::from_pstate(hostile), Nzcv::default());
        assert_eq!(Nzcv::from_pstate(hostile).to_pstate(), 0);
        assert_eq!(Nzcv::from_pstate(u64::MAX).to_pstate(), 0xF000_0000);

        for (bit, expected) in [
            (31, Nzcv { n: true, ..Nzcv::default() }),
            (30, Nzcv { z: true, ..Nzcv::default() }),
            (29, Nzcv { c: true, ..Nzcv::default() }),
            (28, Nzcv { v: true, ..Nzcv::default() }),
        ] {
            let flags = Nzcv::from_pstate(1u64 << bit);
            assert_eq!(flags, expected, "bit {bit}");
            assert_eq!(flags.to_pstate(), 1u64 << bit);
        }
        assert_eq!(Nzcv::from_pstate(u64::MAX).to_string(), "NZCV");
        assert_eq!(Nzcv::default().to_string(), "----");
    }
}
