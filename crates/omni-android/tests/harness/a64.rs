//! The A64 encodings the boundary's tests need, by hand.
//!
//! Committed rather than assembled, so no test depends on an ARM assembler being installed. Each
//! function names the encoding class and its field layout, and
//! [`the_encodings_are_what_they_claim`](tests::the_encodings_are_what_they_claim) states the resulting
//! word in hex, so either side can be checked against the ARM ARM independently.
//!
//! `omni-cpu`'s own test suites carry a sibling of this file. Duplicated rather than shared because a
//! test helper is not a public interface: making `omni-cpu` export one would put an assembler in the
//! CPU crate's API surface to save a hundred lines here, and the hex assertions are what keep the two
//! honest about the same architecture.

#![allow(dead_code)]

use omni_cpu::GuestAddr;

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

/// `ADD Xd, Xn, #imm12` — `1 0 0 100010 0 imm12:12 Rn:5 Rd:5`. `31` is `SP`.
pub const fn add_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0x9100_0000 | (imm12 << 10) | (rn << 5) | rd
}

/// `SUB Xd, Xn, #imm12`.
pub const fn sub_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0xD100_0000 | (imm12 << 10) | (rn << 5) | rd
}

/// `SUB Xd, Xn, Xm` — subtract (shifted register), no shift.
pub const fn sub_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0xCB00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `ADD Xd, Xn, Xm`.
pub const fn add_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0x8B00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `MOV Xd, Xm`, which is `ORR Xd, XZR, Xm`.
pub const fn mov_reg(rd: u32, rm: u32) -> u32 {
    0xAA00_0000 | (rm << 16) | (31 << 5) | rd
}

/// `SUBS Xd, Xn, #imm12` — sets the flags.
pub const fn subs_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0xF100_0000 | (imm12 << 10) | (rn << 5) | rd
}

/// `B.<cond> offset` — `0101 0100 imm19:19 0 cond:4`. Offset in instructions.
pub const fn b_cond(cond: u32, offset_insns: i32) -> u32 {
    0x5400_0000 | (((offset_insns as u32) & 0x7FFFF) << 5) | cond
}

/// `B offset` — `000101 imm26:26`. Offset in instructions.
pub const fn b(offset_insns: i32) -> u32 {
    0x1400_0000 | ((offset_insns as u32) & 0x03FF_FFFF)
}

/// `BL offset` — `100101 imm26:26`. Offset in instructions.
pub const fn bl(offset_insns: i32) -> u32 {
    0x9400_0000 | ((offset_insns as u32) & 0x03FF_FFFF)
}

/// `BR Xn`.
pub const fn br(rn: u32) -> u32 {
    0xD61F_0000 | (rn << 5)
}

/// `BLR Xn` — `1101011 0 0 01 11111 0000 0 0 Rn:5 00000`. An *indirect* call, which is what a PLT
/// stub and a call through a function pointer both are.
pub const fn blr(rn: u32) -> u32 {
    0xD63F_0000 | (rn << 5)
}

/// `RET Xn`.
pub const fn ret(rn: u32) -> u32 {
    0xD65F_0000 | (rn << 5)
}

/// `LDR Xt, [Xn, #byte_offset]` — unsigned offset, scaled by 8.
pub const fn ldr_imm(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xF940_0000 | ((byte_offset / 8) << 10) | (rn << 5) | rt
}

/// `STR Xt, [Xn, #byte_offset]` — unsigned offset, scaled by 8.
pub const fn str_imm(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xF900_0000 | ((byte_offset / 8) << 10) | (rn << 5) | rt
}

/// `LDR Wt, [Xn, #byte_offset]` — 32-bit, scaled by 4.
pub const fn ldr_w(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xB940_0000 | ((byte_offset / 4) << 10) | (rn << 5) | rt
}

/// `STR Wt, [Xn, #byte_offset]` — 32-bit, scaled by 4.
pub const fn str_w(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xB900_0000 | ((byte_offset / 4) << 10) | (rn << 5) | rt
}

/// `LDR Dt, [Xn, #byte_offset]` — SIMD&FP, 64-bit: `size = 11`, `opc = 01`, `imm12` scaled by 8.
pub const fn ldr_d(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xFD40_0000 | ((byte_offset / 8) << 10) | (rn << 5) | rt
}

/// `STR Dt, [Xn, #byte_offset]` — as [`ldr_d`] with `opc = 00`.
pub const fn str_d(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xFD00_0000 | ((byte_offset / 8) << 10) | (rn << 5) | rt
}

/// `LDR St, [Xn, #byte_offset]` — SIMD&FP, 32-bit: `size = 10`, `imm12` scaled by 4.
pub const fn ldr_s(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xBD40_0000 | ((byte_offset / 4) << 10) | (rn << 5) | rt
}

/// `STR St, [Xn, #byte_offset]`.
pub const fn str_s(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xBD00_0000 | ((byte_offset / 4) << 10) | (rn << 5) | rt
}

/// `LDR Qt, [Xn, #byte_offset]` — 128-bit: `size = 00`, `opc = 11`, `imm12` scaled by 16.
pub const fn ldr_q(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0x3DC0_0000 | ((byte_offset / 16) << 10) | (rn << 5) | rt
}

/// `STR Qt, [Xn, #byte_offset]` — as [`ldr_q`] with `opc = 10`.
pub const fn str_q(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0x3D80_0000 | ((byte_offset / 16) << 10) | (rn << 5) | rt
}

/// `FADD Dd, Dn, Dm` — `0001 1110 type:2 1 Rm:5 0010 10 Rn:5 Rd:5`, `type = 01` for double.
pub const fn fadd_d(rd: u32, rn: u32, rm: u32) -> u32 {
    0x1E60_2800 | (rm << 16) | (rn << 5) | rd
}

/// `FMUL Dd, Dn, Dm`.
pub const fn fmul_d(rd: u32, rn: u32, rm: u32) -> u32 {
    0x1E60_0800 | (rm << 16) | (rn << 5) | rd
}

/// `FCVT Dd, Sn` — single to double. `0001 1110 00 1 00010 01 0000 Rn:5 Rd:5`.
///
/// The instruction a guest compiler emits at every `printf("%f", aFloat)` call site: the C default
/// argument promotions convert a `float` in the variadic part to a `double` before the call.
pub const fn fcvt_d_s(rd: u32, rn: u32) -> u32 {
    0x1E22_C000 | (rn << 5) | rd
}

/// `NOP`.
pub const NOP: u32 = 0xD503_201F;

/// The four instructions of an AArch64 PLT stub, as `lld` emits them:
///
/// ```text
///     ADRP X16, got_page
///     LDR  X17, [X16, #got_offset]
///     ADD  X16, X16, #got_offset
///     BR   X17
/// ```
///
/// The shape the real loader produces, and **not** free: four more guest instructions, and the block
/// leaves through `BR` — an *indirect* terminal, which under `optimization::INTERRUPTIBLE` is a
/// dispatcher round trip rather than a linked jump. Included because the boundary has to work through
/// it, not only through a direct `BL`.
pub fn plt_stub(stub_at: GuestAddr, got_slot: GuestAddr) -> Vec<u32> {
    let page_delta = ((got_slot & !0xFFF) as i64 - (stub_at & !0xFFF) as i64) / 0x1000;
    let offset = (got_slot & 0xFFF) as u32;
    vec![adrp(16, page_delta as i32), ldr_imm(17, 16, offset), add_imm(16, 16, offset), br(17)]
}

/// `ADRP Xd, #pages` — `1 immlo:2 10000 immhi:19 Rd:5`, `pages` being the signed 4 KiB page delta.
pub const fn adrp(rd: u32, pages: i32) -> u32 {
    let imm = (pages as u32) & 0x1F_FFFF;
    0x9000_0000 | ((imm & 3) << 29) | ((imm >> 2) << 5) | rd
}

/// A `BL` from `from` to `to`, both guest addresses.
///
/// Computed from both rather than from an assumed program start, because these tests pack several
/// programs into one code region.
pub fn bl_to(from: GuestAddr, to: GuestAddr) -> u32 {
    let delta = to as i64 - from as i64;
    assert!(delta % 4 == 0, "a branch target must be word-aligned");
    let insns = delta / 4;
    assert!((-(1 << 25)..(1 << 25)).contains(&insns), "the BL displacement does not reach");
    bl(insns as i32)
}

/// A `B` from `from` to `to`.
pub fn b_to(from: GuestAddr, to: GuestAddr) -> u32 {
    let delta = to as i64 - from as i64;
    assert!(delta % 4 == 0, "a branch target must be word-aligned");
    b((delta / 4) as i32)
}

/// `MRS Xt, TPIDR_EL0` — `S3_3_C13_C0_2`, the bionic thread pointer.
///
/// The instruction D13 is about: `libroblox.so` holds 1,282 of these and 1,276 of them go
/// straight on to load `[Xt, #0x28]`, which is bionic's `TLS_SLOT_STACK_GUARD`. A guest thread
/// with no thread pointer faults on the first stack-protected call it makes.
pub const fn mrs_tpidr_el0(rt: u32) -> u32 {
    0xD53B_D040 | (rt & 0x1F)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every encoding these tests rely on, in hex, so it can be checked against the ARM ARM by eye
    /// rather than trusted. A wrong encoding here would make a boundary test assert something about an
    /// instruction nobody meant to write.
    #[test]
    fn the_encodings_are_what_they_claim() {
        assert_eq!(movz(0, 0x1234, 0), 0xD282_4680);
        assert_eq!(movk(0, 1, 3), 0xF2E0_0020);
        assert_eq!(add_imm(1, 1, 1), 0x9100_0421);
        assert_eq!(sub_imm(31, 31, 16), 0xD100_43FF, "SUB SP, SP, #16");
        assert_eq!(add_imm(31, 31, 16), 0x9100_43FF, "ADD SP, SP, #16");
        assert_eq!(sub_reg(0, 1, 2), 0xCB02_0020, "SUB X0, X1, X2");
        assert_eq!(add_reg(0, 1, 2), 0x8B02_0020, "ADD X0, X1, X2");
        assert_eq!(mov_reg(21, 30), 0xAA1E_03F5, "MOV X21, X30");
        assert_eq!(subs_imm(19, 19, 1), 0xF100_0673);
        assert_eq!(b(0), 0x1400_0000);
        assert_eq!(b(-1), 0x17FF_FFFF);
        assert_eq!(bl(1), 0x9400_0001);
        assert_eq!(bl(-1), 0x97FF_FFFF);
        assert_eq!(b_cond(1, -2), 0x54FF_FFC1, "B.NE -8");
        assert_eq!(br(17), 0xD61F_0220);
        assert_eq!(blr(9), 0xD63F_0120, "BLR X9");
        assert_eq!(ret(30), 0xD65F_03C0);
        // `BR` and `BLR` differ only in the op field: one sets the link register and one does not,
        // which is the difference between a tail call and a call.
        assert_eq!(br(9) ^ blr(9), 0x0020_0000);
        assert_eq!(ldr_imm(1, 0, 0x28), 0xF940_1401, "LDR X1, [X0, #0x28]");
        assert_eq!(str_imm(2, 1, 8), 0xF900_0422, "STR X2, [X1, #8]");
        assert_eq!(ldr_w(0, 1, 4), 0xB940_0420, "LDR W0, [X1, #4]");
        assert_eq!(str_w(0, 1, 4), 0xB900_0420, "STR W0, [X1, #4]");
        assert_eq!(ldr_d(0, 1, 0), 0xFD40_0020, "LDR D0, [X1]");
        assert_eq!(str_d(2, 1, 16), 0xFD00_0822, "STR D2, [X1, #16]");
        assert_eq!(ldr_s(0, 1, 4), 0xBD40_0420, "LDR S0, [X1, #4]");
        assert_eq!(str_s(0, 1, 4), 0xBD00_0420, "STR S0, [X1, #4]");
        assert_eq!(ldr_q(0, 1, 0), 0x3DC0_0020, "LDR Q0, [X1]");
        assert_eq!(str_q(1, 0, 16), 0x3D80_0401, "STR Q1, [X0, #16]");
        // Load versus store is one bit in `opc`, and getting it wrong produces a legal instruction of
        // the opposite direction.
        assert_eq!(ldr_q(0, 1, 0) ^ str_q(0, 1, 0), 0x0040_0000);
        assert_eq!(fadd_d(2, 0, 1), 0x1E61_2802, "FADD D2, D0, D1");
        assert_eq!(fmul_d(2, 0, 1), 0x1E61_0802, "FMUL D2, D0, D1");
        assert_eq!(fcvt_d_s(0, 0), 0x1E22_C000, "FCVT D0, S0");
        assert_eq!(NOP, 0xD503_201F);
        assert_eq!(mov64(3, 0x1_0000_0000), vec![movz(3, 1, 2)]);
        assert_eq!(mov64(0, 0), vec![movz(0, 0, 0)], "zero is one MOVZ, not nothing");
    }

    #[test]
    fn a_branch_helper_computes_its_displacement_from_both_addresses() {
        assert_eq!(bl_to(0x1000, 0x1004), bl(1));
        assert_eq!(bl_to(0x1004, 0x1000), bl(-1));
        assert_eq!(b_to(0x2000, 0x1000), b(-1024));
    }
}
