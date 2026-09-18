//! A64 instruction encoders, by hand.
//!
//! Small enough to check against the ARM ARM by eye, and committed so no test
//! depends on an assembler being installed. Each function names the encoding
//! class and spells out the field layout; the tests that use them also state
//! the resulting word in hex, so a reader can verify either side independently.
//!
//! Register numbering follows the architecture: `31` means `XZR`/`WZR` in the
//! data-processing encodings and `SP` in the load/store and add/sub-immediate
//! encodings.

#![allow(dead_code)]

/// `MOVZ Xd, #imm16, LSL #(16*hw)` — C4.1.93, `sf=1 opc=10 100101`.
/// `1 10 100101 hw:2 imm16:16 Rd:5`
pub const fn movz(rd: u32, imm16: u16, hw: u32) -> u32 {
    0xD280_0000 | (hw << 21) | ((imm16 as u32) << 5) | rd
}

/// `MOVK Xd, #imm16, LSL #(16*hw)` — `sf=1 opc=11 100101`.
pub const fn movk(rd: u32, imm16: u16, hw: u32) -> u32 {
    0xF280_0000 | (hw << 21) | ((imm16 as u32) << 5) | rd
}

/// `MOVN Xd, #imm16, LSL #(16*hw)` -- `sf=1 opc=00 100101`; the result is the
/// bitwise complement of the shifted immediate, so `MOVN Xd, #7` is -8.
pub const fn movn(rd: u32, imm16: u16, hw: u32) -> u32 {
    0x9280_0000 | (hw << 21) | ((imm16 as u32) << 5) | rd
}

/// Load a full 64-bit constant into `Xd`: `MOVZ` for the lowest non-zero
/// 16-bit field, then `MOVK` for each higher non-zero one. A constant with a
/// single non-zero field costs a single instruction, which several tests rely
/// on when they state the encoding.
pub fn mov64(rd: u32, value: u64) -> Vec<u32> {
    let mut out = Vec::new();
    for hw in 0..4u32 {
        let part = ((value >> (16 * hw)) & 0xFFFF) as u16;
        if part == 0 {
            continue;
        }
        if out.is_empty() {
            out.push(movz(rd, part, hw));
        } else {
            out.push(movk(rd, part, hw));
        }
    }
    if out.is_empty() {
        out.push(movz(rd, 0, 0));
    }
    out
}

/// `ADD Xd, Xn, Xm, <shift> #amount` — add/subtract (shifted register),
/// `1 0 0 01011 shift:2 0 Rm:5 imm6:6 Rn:5 Rd:5`.
/// `shift`: 0 = LSL, 1 = LSR, 2 = ASR.
pub const fn add_shifted(rd: u32, rn: u32, rm: u32, shift: u32, amount: u32) -> u32 {
    0x8B00_0000 | (shift << 22) | (rm << 16) | (amount << 10) | (rn << 5) | rd
}

/// `SUB Xd, Xn, Xm, <shift> #amount`.
pub const fn sub_shifted(rd: u32, rn: u32, rm: u32, shift: u32, amount: u32) -> u32 {
    0xCB00_0000 | (shift << 22) | (rm << 16) | (amount << 10) | (rn << 5) | rd
}

/// `ORR Xd, Xn, Xm` — logical (shifted register), no shift.
pub const fn orr_shifted(rd: u32, rn: u32, rm: u32) -> u32 {
    0xAA00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `MOV Xd, Xm`, which is `ORR Xd, XZR, Xm`.
pub const fn mov_reg(rd: u32, rm: u32) -> u32 {
    orr_shifted(rd, 31, rm)
}

/// `AND Xd, Xn, Xm, LSL #amount`.
pub const fn and_shifted(rd: u32, rn: u32, rm: u32, amount: u32) -> u32 {
    0x8A00_0000 | (rm << 16) | (amount << 10) | (rn << 5) | rd
}

/// `EOR Xd, Xn, Xm, LSL #amount`.
pub const fn eor_shifted(rd: u32, rn: u32, rm: u32, amount: u32) -> u32 {
    0xCA00_0000 | (rm << 16) | (amount << 10) | (rn << 5) | rd
}

/// `ADD Xd, Xn, #imm12` — add/subtract (immediate),
/// `1 0 0 100010 sh imm12:12 Rn:5 Rd:5`.
pub const fn add_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0x9100_0000 | (imm12 << 10) | (rn << 5) | rd
}

/// `SUB Xd, Xn, #imm12`.
pub const fn sub_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0xD100_0000 | (imm12 << 10) | (rn << 5) | rd
}

/// `SUBS Xd, Xn, #imm12` — sets `NZCV`. `SUBS XZR, ...` is `CMP`.
pub const fn subs_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0xF100_0000 | (imm12 << 10) | (rn << 5) | rd
}

/// `SUBS Xd, Xn, Xm` — add/subtract (shifted register) with `S=1`.
pub const fn subs_shifted(rd: u32, rn: u32, rm: u32) -> u32 {
    0xEB00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `B.<cond> label` — `0101010 0 imm19:19 0 cond:4`, `offset = imm19 * 4`.
pub const fn b_cond(cond: u32, offset_insns: i32) -> u32 {
    0x5400_0000 | (((offset_insns as u32) & 0x7_FFFF) << 5) | cond
}

/// `B label` — `000101 imm26:26`, `offset = imm26 * 4`.
pub const fn b(offset_insns: i32) -> u32 {
    0x1400_0000 | ((offset_insns as u32) & 0x03FF_FFFF)
}

/// `BL label` — as `B` with bit 31 set; writes the return address to `X30`.
pub const fn bl(offset_insns: i32) -> u32 {
    0x9400_0000 | ((offset_insns as u32) & 0x03FF_FFFF)
}

/// `RET Xn` — `1101011 0 0 10 11111 000000 Rn:5 00000`.
pub const fn ret(rn: u32) -> u32 {
    0xD65F_0000 | (rn << 5)
}

/// `BR Xn` -- an indirect branch, `1101011 0 0 00 11111 000000 Rn:5 00000`.
pub const fn br(rn: u32) -> u32 {
    0xD61F_0000 | (rn << 5)
}

/// `LDR Xt, [Xn, #imm12*8]` — load/store unsigned immediate, `size=11 opc=01`.
pub const fn ldr_imm(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xF940_0000 | ((byte_offset / 8) << 10) | (rn << 5) | rt
}

/// `STR Xt, [Xn, #imm12*8]`.
pub const fn str_imm(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xF900_0000 | ((byte_offset / 8) << 10) | (rn << 5) | rt
}

/// `LDRB Wt, [Xn, #imm12]` — `size=00 opc=01`.
pub const fn ldrb_imm(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0x3940_0000 | (byte_offset << 10) | (rn << 5) | rt
}

/// `STRB Wt, [Xn, #imm12]`.
pub const fn strb_imm(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0x3900_0000 | (byte_offset << 10) | (rn << 5) | rt
}

/// `LDR Xt, [Xn, Xm, LSL #3]` — load/store register offset, `option=011 S=1`.
/// `11 111 0 00 01 1 Rm:5 011 1 10 Rn:5 Rt:5`
pub const fn ldr_reg(rt: u32, rn: u32, rm: u32) -> u32 {
    0xF860_7800 | (rm << 16) | (rn << 5) | rt
}

/// `STR Xt, [Xn, Xm, LSL #3]`.
pub const fn str_reg(rt: u32, rn: u32, rm: u32) -> u32 {
    0xF820_7800 | (rm << 16) | (rn << 5) | rt
}

/// `STP Xt, Xt2, [Xn, #imm7*8]` — load/store pair, signed offset.
/// `10 101 0 010 0 imm7:7 Rt2:5 Rn:5 Rt:5`
pub const fn stp_imm(rt: u32, rt2: u32, rn: u32, byte_offset: i32) -> u32 {
    let imm7 = ((byte_offset / 8) as u32) & 0x7F;
    0xA900_0000 | (imm7 << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// `LDP Xt, Xt2, [Xn, #imm7*8]`.
pub const fn ldp_imm(rt: u32, rt2: u32, rn: u32, byte_offset: i32) -> u32 {
    let imm7 = ((byte_offset / 8) as u32) & 0x7F;
    0xA940_0000 | (imm7 << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// `LDXR Xt, [Xn]` — `11 001000 0 1 0 11111 0 11111 Rn:5 Rt:5`.
pub const fn ldxr(rt: u32, rn: u32) -> u32 {
    0xC85F_7C00 | (rn << 5) | rt
}

/// `STXR Ws, Xt, [Xn]` — `Ws` receives 0 on success.
pub const fn stxr(rs: u32, rt: u32, rn: u32) -> u32 {
    0xC800_7C00 | (rs << 16) | (rn << 5) | rt
}

/// `FMOV Dd, Xn` — `1 0 0 11110 01 1 00111 000000 Rn:5 Rd:5`.
pub const fn fmov_d_from_x(rd: u32, rn: u32) -> u32 {
    0x9E67_0000 | (rn << 5) | rd
}

/// `FMOV Xd, Dn`.
pub const fn fmov_x_from_d(rd: u32, rn: u32) -> u32 {
    0x9E66_0000 | (rn << 5) | rd
}

/// `FADD Dd, Dn, Dm` — `0 0 0 11110 01 1 Rm:5 001 0 10 Rn:5 Rd:5`.
pub const fn fadd_d(rd: u32, rn: u32, rm: u32) -> u32 {
    0x1E60_2800 | (rm << 16) | (rn << 5) | rd
}

/// `FMUL Dd, Dn, Dm`.
pub const fn fmul_d(rd: u32, rn: u32, rm: u32) -> u32 {
    0x1E60_0800 | (rm << 16) | (rn << 5) | rd
}

/// `SCVTF Dd, Xn` — signed 64-bit integer to double.
pub const fn scvtf_d_from_x(rd: u32, rn: u32) -> u32 {
    0x9E62_0000 | (rn << 5) | rd
}

/// `FCVTZS Xd, Dn` — double to signed 64-bit integer, round toward zero.
pub const fn fcvtzs_x_from_d(rd: u32, rn: u32) -> u32 {
    0x9E78_0000 | (rn << 5) | rd
}

/// `ADD Vd.4S, Vn.4S, Vm.4S` — SIMD three-same, `Q=1 size=10 opcode=10000`.
pub const fn add_vec_4s(rd: u32, rn: u32, rm: u32) -> u32 {
    0x4EA0_8400 | (rm << 16) | (rn << 5) | rd
}

/// `ADD Vd.2D, Vn.2D, Vm.2D` — `size=11`.
pub const fn add_vec_2d(rd: u32, rn: u32, rm: u32) -> u32 {
    0x4EE0_8400 | (rm << 16) | (rn << 5) | rd
}

/// `LDR Qt, [Xn, #imm12*16]` — 128-bit load, `size=00 V=1 opc=11`.
pub const fn ldr_q_imm(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0x3DC0_0000 | ((byte_offset / 16) << 10) | (rn << 5) | rt
}

/// `STR Qt, [Xn, #imm12*16]`.
pub const fn str_q_imm(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0x3D80_0000 | ((byte_offset / 16) << 10) | (rn << 5) | rt
}

/// `MRS Xt, TPIDR_EL0` — `S3_3_C13_C0_2`.
pub const fn mrs_tpidr_el0(rt: u32) -> u32 {
    0xD53B_D040 | rt
}

/// `SVC #imm16` — `11010100 000 imm16:16 000 01`.
pub const fn svc(imm16: u16) -> u32 {
    0xD400_0001 | ((imm16 as u32) << 5)
}

/// `NOP`.
pub const NOP: u32 = 0xD503_201F;

/// `BRK #0`.
pub const BRK: u32 = 0xD420_0000;

/// `YIELD`, a hint instruction.
pub const YIELD: u32 = 0xD503_203F;

/// Condition codes for [`b_cond`].
pub mod cond {
    /// Equal (`Z == 1`).
    pub const EQ: u32 = 0;
    /// Not equal (`Z == 0`).
    pub const NE: u32 = 1;
    /// Unsigned higher or same (`C == 1`).
    pub const HS: u32 = 2;
    /// Signed greater than.
    pub const GT: u32 = 12;
}
