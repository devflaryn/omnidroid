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

/// `LDAR Xt, [Xn]` — load-acquire. `11 001000 1 1 0 11111 1 11111 Rn Rt`.
///
/// The dominant atomic-ish class in `libroblox.so`: 15,516 sites, against 128 exclusives and 53
/// LSE (`tools/atomic_mix.py`). It is an ordinary load with ordering, not a read-modify-write, and
/// it touches no exclusive monitor — which is exactly why the first version of the Task 4 report
/// miscounted it as `LDAXR`.
pub const fn ldar(rt: u32, rn: u32) -> u32 {
    0xC8DF_FC00 | (rn << 5) | rt
}

/// `STLR Xt, [Xn]` — store-release. As `LDAR` with `L = 0`.
pub const fn stlr(rt: u32, rn: u32) -> u32 {
    0xC89F_FC00 | (rn << 5) | rt
}

/// `LDXR Xt, [Xn]` — load-exclusive, which *does* take the monitor.
pub const fn ldxr(rt: u32, rn: u32) -> u32 {
    0xC85F_7C00 | (rn << 5) | rt
}

/// `STXR Ws, Xt, [Xn]` — store-exclusive. `Ws` receives 0 on success.
pub const fn stxr(rs: u32, rt: u32, rn: u32) -> u32 {
    0xC800_7C00 | (rs << 16) | (rn << 5) | rt
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


/// `MRS Xt, <system register>` — `1101 0101 0011 o0 op1:3 CRn:4 CRm:4 op2:3 Rt:5`, where
/// `o0 = op0 - 2`. Spelled out rather than given as a constant so the two below can be checked
/// against the ARM ARM field by field.
pub const fn mrs(rt: u32, op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    0xD530_0000
        | ((op0 - 2) << 19)
        | (op1 << 16)
        | (crn << 12)
        | (crm << 8)
        | (op2 << 5)
        | rt
}

/// `LDR Qt, [Xn, #byte_offset]` — SIMD&FP load, unsigned immediate, **128-bit**.
///
/// `size:2 111 V=1 01 opc:2 imm12 Rn Rt`, with `size = 00` and `opc = 11` selecting the 128-bit form
/// and `imm12` scaled by 16. Spelled out because the width lives in two separate fields and getting
/// it wrong produces a legal instruction of the wrong size.
pub const fn ldr_q(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0x3DC0_0000 | ((byte_offset / 16) << 10) | (rn << 5) | rt
}

/// `STR Qt, [Xn, #byte_offset]` — as [`ldr_q`] with `opc = 10`.
pub const fn str_q(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0x3D80_0000 | ((byte_offset / 16) << 10) | (rn << 5) | rt
}

/// `LDR Dt, [Xn, #byte_offset]` — SIMD&FP load, 64-bit: `size = 11`, `opc = 01`, `imm12` scaled by 8.
pub const fn ldr_d(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xFD40_0000 | ((byte_offset / 8) << 10) | (rn << 5) | rt
}

/// `STR Dt, [Xn, #byte_offset]` — as [`ldr_d`] with `opc = 00`.
pub const fn str_d(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xFD00_0000 | ((byte_offset / 8) << 10) | (rn << 5) | rt
}

/// `FMUL Dd, Dn, Dm` — floating-point data processing, two source, double precision.
///
/// `0001 1110 type:2 1 Rm:5 0000 10 Rn:5 Rd:5`, `type = 01` for double. Used to ask a
/// *denormal-sensitive* question of the guest's `FPCR`, which is the only way to observe from guest
/// code whether its floating-point control state survived a call out of the guest world.
pub const fn fmul_d(rd: u32, rn: u32, rm: u32) -> u32 {
    0x1E60_0800 | (rm << 16) | (rn << 5) | rd
}

/// `MSR <system register>, Xt` — as [`mrs`] with bit 21 clear (`L = 0`, a write rather than a read).
pub const fn msr(rt: u32, op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    mrs(rt, op0, op1, crn, crm, op2) & !(1 << 21)
}

/// `MSR FPCR, Xt` — `S3_3_C4_C4_0`.
///
/// Used to make the guest's floating-point control state differ from the host's, which is how the
/// MXCSR question is asked: dynarmic maps `FPCR` onto `guest_MXCSR` in `A64JitState::SetFpcr`, and
/// whether that mapping is live inside a host callback is a property of the emitter, not of us.
pub const fn msr_fpcr(rt: u32) -> u32 {
    msr(rt, 3, 3, 4, 4, 0)
}

/// `MRS Xt, FPCR`.
pub const fn mrs_fpcr(rt: u32) -> u32 {
    mrs(rt, 3, 3, 4, 4, 0)
}

/// `MRS Xt, CNTFRQ_EL0` — `S3_3_C14_C0_0`. The frequency the counter below ticks at.
pub const fn mrs_cntfrq_el0(rt: u32) -> u32 {
    mrs(rt, 3, 3, 14, 0, 0)
}

/// `MRS Xt, CNTPCT_EL0` — `S3_3_C14_C0_1`. The architectural counter itself.
pub const fn mrs_cntpct_el0(rt: u32) -> u32 {
    mrs(rt, 3, 3, 14, 0, 1)
}

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
        // `STLR XZR, [X21]` is word 0xc89ffebf, taken straight out of `libroblox.so` -- the exact
        // word the first atomics count misread as a store-exclusive. Checking the helper against a
        // word from the real binary is the only way to be sure these are the engine's encodings
        // and not merely ones that assemble.
        assert_eq!(stlr(31, 21), 0xC89F_FEBF);
        assert_eq!(ldar(1, 0), 0xC8DF_FC01);
        assert_eq!(ldxr(1, 0), 0xC85F_7C01);
        assert_eq!(stxr(2, 1, 0), 0xC802_7C01);
        // `LDAR` and `LDXR` differ only in bit 23 (`o2`, which decides ordered versus exclusive)
        // and bit 15 (`o0`, the acquire flag). Bit 23 is the one the first atomics count did not
        // read, and it is the whole difference between "takes the global monitor" and "does not".
        assert_eq!(ldar(1, 0) ^ ldxr(1, 0), 0x0080_8000);
        assert_eq!((ldar(1, 0) >> 23) & 1, 1, "LDAR is o2 = 1: ordered");
        assert_eq!((ldxr(1, 0) >> 23) & 1, 0, "LDXR is o2 = 0: exclusive");
        assert_eq!(svc(0xFFFF), 0xD41F_FFE1);
        // `MRS X0, FPCR` is 0xd53b4400 and `MSR FPCR, X0` is 0xd51b4400 -- one bit apart, and it is
        // the bit that decides which direction the transfer goes.
        assert_eq!(mrs_fpcr(0), 0xD53B_4400);
        assert_eq!(msr_fpcr(0), 0xD51B_4400);
        assert_eq!(mrs_fpcr(0) ^ msr_fpcr(0), 1 << 21);
        // The two SIMD&FP widths, whose size field is split across two places in the encoding.
        assert_eq!(ldr_q(0, 1, 0), 0x3DC0_0020, "LDR Q0, [X1]");
        assert_eq!(str_q(1, 0, 16), 0x3D80_0401, "STR Q1, [X0, #16]");
        assert_eq!(ldr_d(0, 1, 0), 0xFD40_0020, "LDR D0, [X1]");
        assert_eq!(str_d(2, 1, 16), 0xFD00_0822, "STR D2, [X1, #16]");
        assert_eq!(ldr_q(0, 1, 0) ^ str_q(0, 1, 0), 0x0040_0000, "load vs store is opc bit 22");
        assert_eq!(fmul_d(2, 0, 1), 0x1E61_0802, "FMUL D2, D0, D1");
        assert_eq!(brk(0), 0xD420_0000);
        assert_eq!(mov64(3, 0x1_0000_0000), vec![movz(3, 1, 2)]);
    }
}
