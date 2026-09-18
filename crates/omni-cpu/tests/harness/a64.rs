//! The A64 encodings these tests need, by hand.
//!
//! Committed rather than assembled, so no test depends on an ARM assembler being installed. Each
//! function names the encoding class and spells out the field layout, and the tests that use them
//! state the resulting word in hex, so either side can be checked independently against the ARM ARM.
//!
//! Register numbering follows the architecture: `31` is `XZR`/`WZR` in the data-processing
//! encodings and `SP` in the load/store and add/sub-immediate ones.

#![allow(dead_code)]

/// `MOVZ Xd, #imm16, LSL #(16*hw)` — `1 10 100101 hw:2 imm16:16 Rd:5`.
pub const fn movz(rd: u32, imm16: u16, hw: u32) -> u32 {
    0xD280_0000 | (hw << 21) | ((imm16 as u32) << 5) | rd
}

/// `MOVK Xd, #imm16, LSL #(16*hw)` — `1 11 100101 hw:2 imm16:16 Rd:5`.
pub const fn movk(rd: u32, imm16: u16, hw: u32) -> u32 {
    0xF280_0000 | (hw << 21) | ((imm16 as u32) << 5) | rd
}

/// Load a full 64-bit constant: `MOVZ` for the lowest non-zero halfword, `MOVK` for each higher one.
pub fn mov64(rd: u32, value: u64) -> Vec<u32> {
    let mut out = Vec::new();
    for hw in 0..4u32 {
        let part = ((value >> (16 * hw)) & 0xFFFF) as u16;
        if part == 0 {
            continue;
        }
        out.push(if out.is_empty() { movz(rd, part, hw) } else { movk(rd, part, hw) });
    }
    if out.is_empty() {
        out.push(movz(rd, 0, 0));
    }
    out
}

/// `ADD Xd, Xn, #imm12` — `1 0 0 100010 0 imm12:12 Rn:5 Rd:5`.
pub const fn add_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0x9100_0000 | (imm12 << 10) | (rn << 5) | rd
}

/// `SUBS Xd, Xn, #imm12` — sets the flags. `1 1 1 100010 0 imm12:12 Rn:5 Rd:5`.
pub const fn subs_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0xF100_0000 | (imm12 << 10) | (rn << 5) | rd
}

/// `ADD Xd, Xn, Xm` — add/subtract (shifted register), no shift.
pub const fn add_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0x8B00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `MOV Xd, Xm`, which is `ORR Xd, XZR, Xm`.
pub const fn mov_reg(rd: u32, rm: u32) -> u32 {
    0xAA00_0000 | (rm << 16) | (31 << 5) | rd
}

/// `B.<cond> offset` — `0101 0100 imm19:19 0 cond:4`. Offset in instructions.
pub const fn b_cond(cond: u32, offset_insns: i32) -> u32 {
    0x5400_0000 | (((offset_insns as u32) & 0x7FFFF) << 5) | cond
}

/// `B offset` — `000101 imm26:26`. Offset in instructions.
pub const fn b(offset_insns: i32) -> u32 {
    0x1400_0000 | ((offset_insns as u32) & 0x03FF_FFFF)
}

/// `BR Xn` — `1101011 0 0 00 11111 0000 0 0 Rn:5 00000`.
pub const fn br(rn: u32) -> u32 {
    0xD61F_0000 | (rn << 5)
}

/// `RET Xn` — `1101011 0 0 10 11111 0000 0 0 Rn:5 00000`.
pub const fn ret(rn: u32) -> u32 {
    0xD65F_0000 | (rn << 5)
}

/// `LDR Xt, [Xn, #byte_offset]` — unsigned offset, scaled by 8. `11 111 0 01 01 imm12 Rn Rt`.
pub const fn ldr_imm(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xF940_0000 | ((byte_offset / 8) << 10) | (rn << 5) | rt
}

/// `STR Xt, [Xn, #byte_offset]` — unsigned offset, scaled by 8.
pub const fn str_imm(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xF900_0000 | ((byte_offset / 8) << 10) | (rn << 5) | rt
}

/// `LDR Xt, [Xn, Xm, LSL #0]` — register offset. `11 111 0 00 01 1 Rm 011 0 10 Rn Rt`.
pub const fn ldr_reg(rt: u32, rn: u32, rm: u32) -> u32 {
    0xF860_6800 | (rm << 16) | (rn << 5) | rt
}

/// `STR Xt, [Xn, Xm, LSL #0]` — register offset.
pub const fn str_reg(rt: u32, rn: u32, rm: u32) -> u32 {
    0xF820_6800 | (rm << 16) | (rn << 5) | rt
}

/// `MRS Xt, TPIDR_EL0` — `1101 0101 0011 1101 1101 0000 011 Rt`, i.e. `op0=3 op1=3 CRn=13 CRm=0
/// op2=2`.
pub const fn mrs_tpidr_el0(rt: u32) -> u32 {
    0xD53B_D040 | rt
}

/// `MRS Xt, TPIDRRO_EL0` — as above with `op2 = 3`.
pub const fn mrs_tpidrro_el0(rt: u32) -> u32 {
    0xD53B_D060 | rt
}

/// `MSR TPIDR_EL0, Xt`.
pub const fn msr_tpidr_el0(rt: u32) -> u32 {
    0xD51B_D040 | rt
}

/// `SVC #imm16` — `1101 0100 000 imm16 00001`.
pub const fn svc(imm16: u16) -> u32 {
    0xD400_0001 | ((imm16 as u32) << 5)
}

/// `BRK #imm16` — `1101 0100 001 imm16 00000`.
pub const fn brk(imm16: u16) -> u32 {
    0xD420_0000 | ((imm16 as u32) << 5)
}

/// `NOP`.
pub const NOP: u32 = 0xD503_201F;

/// An encoding no A64 decoder allocates, for testing the unsupported-instruction exit.
///
/// The whole `0x0000_0000` … `0x0000_FFFF` space is UNALLOCATED in the ARM ARM's top-level
/// decode (op0 = 0000 with a non-zero remainder is reserved), and dynarmic's decoder reports it as
/// `UnallocatedEncoding`.
pub const UNALLOCATED: u32 = 0x0000_0001;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every encoding these tests rely on, stated in hex so it can be checked by eye.
    #[test]
    fn the_encodings_are_what_they_claim() {
        assert_eq!(movz(0, 0x1234, 0), 0xD282_4680);
        assert_eq!(add_imm(1, 1, 1), 0x9100_0421);
        assert_eq!(subs_imm(2, 2, 1), 0xF100_0442);
        assert_eq!(b(0), 0x1400_0000, "B . -- offset 0 is a branch to the branch itself");
        assert_eq!(b(-1), 0x17FF_FFFF, "B #-4, one instruction back");
        assert_eq!(b_cond(1, -2), 0x54FF_FFC1, "B.NE -8");
        assert_eq!(br(30), 0xD61F_03C0);
        assert_eq!(ret(30), 0xD65F_03C0);
        assert_eq!(ldr_imm(1, 0, 0x28), 0xF940_1401, "LDR X1, [X0, #0x28]");
        assert_eq!(mrs_tpidr_el0(0), 0xD53B_D040);
        assert_eq!(svc(0xFFFF), 0xD41F_FFE1);
        assert_eq!(brk(0), 0xD420_0000);
        assert_eq!(mov64(3, 0x1_0000_0000), vec![movz(3, 1, 2)]);
    }
}
