//! **Half-precision (FEAT_FP16) arithmetic**, scalar and vector, executed from guest code.
//!
//! Every instruction here reaches an IR opcode the pin's **arm64** backend implemented as
//! `ASSERT_FALSE("Unimplemented")` -- `FPAbs16`, `FPNeg16`, `FPMulAdd16`, `FPMulSub16`,
//! `FPRoundInt16`, `FPRecipEstimate16`, `FPRecipExponent16`, `FPRSqrtEstimate16`,
//! `FPRecipStepFused16`, `FPRSqrtStepFused16`, `FPHalfToFixed{S,U}{32,64}` (scalar) and
//! `FPVectorEqual16`, `FPVectorMulAdd16`, `FPVectorNeg16`, `FPVectorRecipEstimate16`,
//! `FPVectorRSqrtEstimate16`, `FPVectorRecipStepFused16`, `FPVectorRSqrtStepFused16` (vector) -- so
//! each one terminated the process on an arm64 host. `FRINTN`/`FRINTA` (vector, `.8H`) reach
//! `FPVectorRoundInt16`, which *was* implemented, through a fallback helper whose register-save mask
//! named the result register's **GPR** twin: the result was restored from the stack over the
//! computed value, a silent wrong answer. Patch 0004 covers both.
//!
//! **Encodings**: each hex word below was checked against the LLVM assembler
//! (`clang -march=armv8.4-a+fp16`, `objdump -d`), which is independent of dynarmic's decoder table.
//!
//! **Oracle: IEEE 754 binary16 and the ARM ARM pseudocode, worked by hand** (VERIFICATION entry
//! 7). Constants: `1.0 = 0x3C00`, `1.5 = 0x3E00`, `2.0 = 0x4000`, `2.5 = 0x4100`, `3.0 = 0x4200`,
//! `4.0 = 0x4400`, `5.0 = 0x4500`, `7.0 = 0x4700`, `0.5 = 0x3800`, `+inf = 0x7C00`, sign bit `0x8000`,
//! default NaN `0x7E00`. The reciprocal and reciprocal-square-root estimates are the ARM ARM's 8-bit
//! `RecipEstimate`/`RecipSqrtEstimate`, the same table at every precision: for an input of exactly
//! 1.0 the table gives `511/512 = 0.998046875`, which in binary16 is exponent -1 (biased 14) and
//! fraction `1020/1024`, i.e. `0x3BFC`; halving the result (input 2.0 for `FRECPE`, 4.0 for
//! `FRSQRTE`) takes one off the exponent, `0x37FC`.

mod harness;

use harness::a64;
use harness::{Vm, VmOptions, HALT_DONE};

/// `FPSR.IOC` (invalid operation), `FPSR.DZC` (divide by zero), `FPSR.IXC` (inexact).
const IOC: u64 = 1 << 0;
const DZC: u64 = 1 << 1;
const IXC: u64 = 1 << 4;

/// Output of one run: `V0`, `X0` and `FPSR`.
struct Out {
    v0: [u64; 2],
    x0: u64,
    fpsr: u64,
}

/// `op ; MRS X9, FPSR ; SVC #0`, with `V0`..`V3` set as given (anything not given is zero, and `V0`
/// all ones so a scalar write that fails to clear the upper bits shows).
fn run(op: u32, v: &[[u64; 2]]) -> Out {
    let code = vec![op, a64::mrs_fpsr(9), a64::svc(0)];
    let vm = Vm::new(code, VmOptions::default());
    vm.set_vec(0, [u64::MAX, u64::MAX]);
    for (i, value) in v.iter().enumerate() {
        vm.set_vec(i as u32, *value);
    }
    vm.start(1_000_000);
    let hr = vm.run_to_completion(16);
    assert_eq!(hr & HALT_DONE, HALT_DONE, "{op:#010X}: the guest did not reach its SVC (halt {hr:#010X})");
    Out { v0: vm.vec(0), x0: vm.reg(0), fpsr: vm.reg(9) }
}

/// A scalar `H` op on `H1` (and `H2`, `H3`): the result in `H0`, the rest of `V0` cleared.
fn scalar(name: &str, op: u32, h1: u16, h2: u16, h3: u16, want: u16) -> u64 {
    // V0 is overwritten by the op; V1..V3 carry junk above the element, which must be ignored.
    let junk = 0xA5A5_A5A5_A5A5_0000u64;
    let out = run(
        op,
        &[
            [u64::MAX, u64::MAX],
            [junk | u64::from(h1), u64::MAX],
            [junk | u64::from(h2), u64::MAX],
            [junk | u64::from(h3), u64::MAX],
        ],
    );
    assert_eq!(out.v0, [u64::from(want), 0], "{name}: H0 = {:#06x}, want {want:#06x}", out.v0[0]);
    out.fpsr
}

/// Eight lanes into one `u64` pair.
fn lanes(h: [u16; 8]) -> [u64; 2] {
    let mut out = [0u64; 2];
    for (i, lane) in h.iter().enumerate() {
        out[i / 4] |= u64::from(*lane) << (16 * (i % 4));
    }
    out
}

#[test]
fn fabs_and_fneg_touch_only_the_sign_bit() {
    // FABS H0, H1 (1EE0C020): -1.0 -> 1.0; a negative quiet NaN keeps its payload, sign cleared.
    assert_eq!(scalar("FABS -1", 0x1EE0_C020, 0xBC00, 0, 0, 0x3C00) & IOC, 0);
    assert_eq!(scalar("FABS -qNaN", 0x1EE0_C020, 0xFE01, 0, 0, 0x7E01) & IOC, 0, "no exception");
    // FNEG H0, H1 (1EE14020): 1.0 -> -1.0, -0.0 -> +0.0.
    scalar("FNEG 1", 0x1EE1_4020, 0x3C00, 0, 0, 0xBC00);
    scalar("FNEG -0", 0x1EE1_4020, 0x8000, 0, 0, 0x0000);
}

#[test]
fn the_four_fused_multiply_adds() {
    // H1 = 2.0, H2 = 3.0, H3 = 1.0.
    // FMADD  H0,H1,H2,H3 (1FC20C20):  H3 + H1*H2 =  1 + 6 =  7.0 = 0x4700
    // FMSUB  H0,H1,H2,H3 (1FC28C20):  H3 - H1*H2 =  1 - 6 = -5.0 = 0xC500
    // FNMADD H0,H1,H2,H3 (1FE20C20): -H3 - H1*H2 = -1 - 6 = -7.0 = 0xC700
    // FNMSUB H0,H1,H2,H3 (1FE28C20): -H3 + H1*H2 = -1 + 6 =  5.0 = 0x4500
    scalar("FMADD", 0x1FC2_0C20, 0x4000, 0x4200, 0x3C00, 0x4700);
    scalar("FMSUB", 0x1FC2_8C20, 0x4000, 0x4200, 0x3C00, 0xC500);
    scalar("FNMADD", 0x1FE2_0C20, 0x4000, 0x4200, 0x3C00, 0xC700);
    scalar("FNMSUB", 0x1FE2_8C20, 0x4000, 0x4200, 0x3C00, 0x4500);
}

#[test]
fn fmadd_rounds_once() {
    // (1 + 2^-10)^2 = 1 + 2^-9 + 2^-20, and the addend is -(1 + 2^-9) = 0xBC02. Fused: exactly
    // 2^-20, a binary16 subnormal (2^-24 * 16 = 0x0010, FPCR.FZ16 is clear). Rounding the product
    // to binary16 first would lose the 2^-20 and give +0.
    scalar("FMADD fused", 0x1FC2_0C20, 0x3C01, 0x3C01, 0xBC02, 0x0010);
}

#[test]
fn frint_in_every_rounding_mode() {
    // 2.5 = 0x4100, -2.5 = 0xC100.
    scalar("FRINTN 2.5 (ties to even)", 0x1EE4_4020, 0x4100, 0, 0, 0x4000);
    scalar("FRINTA 2.5 (ties away)", 0x1EE6_4020, 0x4100, 0, 0, 0x4200);
    scalar("FRINTP 2.5 (to +inf)", 0x1EE4_C020, 0x4100, 0, 0, 0x4200);
    scalar("FRINTM -2.5 (to -inf)", 0x1EE5_4020, 0xC100, 0, 0, 0xC200);
    scalar("FRINTZ -2.5 (to zero)", 0x1EE5_C020, 0xC100, 0, 0, 0xC000);
    // FRINTI uses FPCR.RMode, which is round-to-nearest-even at reset.
    scalar("FRINTI 2.5", 0x1EE7_C020, 0x4100, 0, 0, 0x4000);
    // FRINTX is FRINTI that raises Inexact when the value changed.
    let fpsr = scalar("FRINTX 2.5", 0x1EE7_4020, 0x4100, 0, 0, 0x4000);
    assert_ne!(fpsr & IXC, 0, "FRINTX 2.5 -> 2.0 is inexact: FPSR {fpsr:#x}");
    let fpsr = scalar("FRINTN 2.5", 0x1EE4_4020, 0x4100, 0, 0, 0x4000);
    assert_eq!(fpsr & IXC, 0, "only FRINTX raises Inexact: FPSR {fpsr:#x}");
}

#[test]
fn reciprocal_and_rsqrt_estimates_and_exponent() {
    scalar("FRECPE 1.0", 0x5EF9_D820, 0x3C00, 0, 0, 0x3BFC);
    scalar("FRECPE 2.0", 0x5EF9_D820, 0x4000, 0, 0, 0x37FC);
    // 1/+0 = +inf, and Divide-by-Zero is raised.
    let fpsr = scalar("FRECPE +0", 0x5EF9_D820, 0x0000, 0, 0, 0x7C00);
    assert_ne!(fpsr & DZC, 0, "FRECPE(+0) raises DZC: FPSR {fpsr:#x}");
    scalar("FRECPE +inf", 0x5EF9_D820, 0x7C00, 0, 0, 0x0000);
    scalar("FRSQRTE 1.0", 0x7EF9_D820, 0x3C00, 0, 0, 0x3BFC);
    scalar("FRSQRTE 4.0", 0x7EF9_D820, 0x4400, 0, 0, 0x37FC);
    // FRECPX: sign kept, exponent bitwise inverted, fraction zeroed. 3.0 has biased exponent 16
    // (0b10000) -> 0b01111 = 15, i.e. 1.0; 0.5 (14 = 0b01110) -> 0b10001 = 17, i.e. 4.0. For a zero
    // the result exponent is the largest finite one, 0b11110: 0x7800.
    scalar("FRECPX 3.0", 0x5EF9_F820, 0x4200, 0, 0, 0x3C00);
    scalar("FRECPX 0.5", 0x5EF9_F820, 0x3800, 0, 0, 0x4400);
    scalar("FRECPX -3.0", 0x5EF9_F820, 0xC200, 0, 0, 0xBC00);
    scalar("FRECPX +0", 0x5EF9_F820, 0x0000, 0, 0, 0x7800);
}

#[test]
fn newton_raphson_step_instructions() {
    // FRECPS  = 2.0 - a*b:       (1.0, 1.5) -> 0.5
    // FRSQRTS = (3.0 - a*b)/2:   (1.0, 1.0) -> 1.0 ; (2.0, 1.0) -> 0.5
    // and the ARM ARM's special case for 0 * inf: FRECPS gives 2.0, FRSQRTS gives 1.5.
    scalar("FRECPS 1,1.5", 0x5E42_3C20, 0x3C00, 0x3E00, 0, 0x3800);
    scalar("FRECPS 0,inf", 0x5E42_3C20, 0x0000, 0x7C00, 0, 0x4000);
    scalar("FRSQRTS 1,1", 0x5EC2_3C20, 0x3C00, 0x3C00, 0, 0x3C00);
    scalar("FRSQRTS 2,1", 0x5EC2_3C20, 0x4000, 0x3C00, 0, 0x3800);
    scalar("FRSQRTS 0,inf", 0x5EC2_3C20, 0x0000, 0x7C00, 0, 0x3E00);
}

#[test]
fn half_to_integer_conversions() {
    let conv = |name: &str, op: u32, h: u16| -> (u64, u64) {
        let out = run(op, &[[0, 0], [u64::from(h), 0]]);
        let _ = name;
        (out.x0, out.fpsr)
    };
    // FCVTZS W0, H1 (1EF80020), round towards zero: 2.5 -> 2 ; -2.5 -> -2 (as a W, zero-extended).
    assert_eq!(conv("FCVTZS W 2.5", 0x1EF8_0020, 0x4100).0, 2);
    assert_eq!(conv("FCVTZS W -2.5", 0x1EF8_0020, 0xC100).0, 0xFFFF_FFFE);
    // FCVTZS X0, H1 (9EF80020): the largest finite half, 65504 = 0x7BFF.
    assert_eq!(conv("FCVTZS X max", 0x9EF8_0020, 0x7BFF).0, 65504);
    // FCVTZS X0 of -2.5 is -2 in all 64 bits.
    assert_eq!(conv("FCVTZS X -2.5", 0x9EF8_0020, 0xC100).0, (-2i64) as u64);
    // FCVTZU W0, H1 (1EF90020): -1.0 has no unsigned value -> 0, Invalid Operation.
    let (x, fpsr) = conv("FCVTZU W -1", 0x1EF9_0020, 0xBC00);
    assert_eq!(x, 0);
    assert_ne!(fpsr & IOC, 0, "FCVTZU(-1.0) raises IOC: FPSR {fpsr:#x}");
    // FCVTZU X0, H1 (9EF90020): 3.0 -> 3.
    assert_eq!(conv("FCVTZU X 3", 0x9EF9_0020, 0x4200).0, 3);
    // FCVTNS W0, H1 (1EE00020): 2.5 -> 2 (ties to even); FCVTAS W0, H1 (1EE40020): 2.5 -> 3.
    assert_eq!(conv("FCVTNS 2.5", 0x1EE0_0020, 0x4100).0, 2);
    assert_eq!(conv("FCVTAS 2.5", 0x1EE4_0020, 0x4100).0, 3);
    // FCVTZS W0, H1, #4 (1ED8F020): fixed point with 4 fraction bits, 2.5 * 16 = 40.
    assert_eq!(conv("FCVTZS W 2.5 #4", 0x1ED8_F020, 0x4100).0, 40);
    // Inexact: 2.5 -> 2 loses the half.
    assert_ne!(conv("FCVTZS inexact", 0x1EF8_0020, 0x4100).1 & IXC, 0);
}

#[test]
fn fcmeq_half_scalar_and_vector() {
    // FCMEQ H0, H1, H2 (5E422420): 1.0 == 1.0 -> all ones.
    scalar("FCMEQ equal", 0x5E42_2420, 0x3C00, 0x3C00, 0, 0xFFFF);
    scalar("FCMEQ differ", 0x5E42_2420, 0x3C00, 0x4000, 0, 0x0000);
    // FCMEQ V0.8H, V1.8H, V2.8H (4E422420), lane by lane:
    //   1.0 == 1.0 -> FFFF ; 1.0 == 2.0 -> 0 ; qNaN == qNaN -> 0 (unordered) ; +0 == -0 -> FFFF ;
    //   inf == inf -> FFFF ; -1 == 1 -> 0 ; 7 == 7 -> FFFF ; 0.5 == 4 -> 0
    let out = run(
        0x4E42_2420,
        &[
            [0, 0],
            lanes([0x3C00, 0x3C00, 0x7E00, 0x0000, 0x7C00, 0xBC00, 0x4700, 0x3800]),
            lanes([0x3C00, 0x4000, 0x7E00, 0x8000, 0x7C00, 0x3C00, 0x4700, 0x4400]),
        ],
    );
    assert_eq!(out.v0, lanes([0xFFFF, 0, 0, 0xFFFF, 0xFFFF, 0, 0xFFFF, 0]));
    // A quiet NaN compared for equality is not an Invalid Operation (only a signalling one is).
    assert_eq!(out.fpsr & IOC, 0, "FPSR {:#x}", out.fpsr);
}

#[test]
fn fmla_and_fmls_half_vector() {
    // V0 = 1.0 in every lane, V1 = 2.0, V2 = 3.0 -- except lane 6, where V1 = -2.0 (so a negation
    // that merely *sets* the sign bit is told from one that flips it), and lane 7, which carries the
    // fused case (V0 = -(1 + 2^-9), V1 = V2 = 1 + 2^-10), as in `fmadd_rounds_once`.
    let v0 = lanes([0x3C00, 0x3C00, 0x3C00, 0x3C00, 0x3C00, 0x3C00, 0x3C00, 0xBC02]);
    let v1 = lanes([0x4000, 0x4000, 0x4000, 0x4000, 0x4000, 0x4000, 0xC000, 0x3C01]);
    let v2 = lanes([0x4200, 0x4200, 0x4200, 0x4200, 0x4200, 0x4200, 0x4200, 0x3C01]);
    // FMLA V0.8H, V1.8H, V2.8H (4E420C20): V0 + V1*V2 = 7.0 ; lane 6: 1 + (-6) = -5.0 ;
    // lane 7: 2^-20 = 0x0010.
    let out = run(0x4E42_0C20, &[v0, v1, v2]);
    assert_eq!(out.v0, lanes([0x4700, 0x4700, 0x4700, 0x4700, 0x4700, 0x4700, 0xC500, 0x0010]));
    // FMLS V0.8H, V1.8H, V2.8H (4EC20C20): V0 - V1*V2 = -5.0 ; lane 6: 1 - (-6) = 7.0 ;
    // lane 7: -(1+2^-9) - (1+2^-9+2^-20) = -(2 + 2^-8 + 2^-20), which rounds to nearest in binary16
    // (exponent 1, ulp 2^-9) as -(2 + 2^-8) = -(1 + 2^-9) * 2 -> biased exponent 16, fraction 2 ->
    // 0xC002.
    let out = run(0x4EC2_0C20, &[v0, v1, v2]);
    assert_eq!(out.v0, lanes([0xC500, 0xC500, 0xC500, 0xC500, 0xC500, 0xC500, 0x4700, 0xC002]));
}

#[test]
fn half_vector_estimates_and_steps() {
    let ones = lanes([0x3C00; 8]);
    // FRECPE V0.8H, V1.8H (4EF9D820): lanes 1.0 and 2.0 alternate -> 0x3BFC, 0x37FC.
    let x = lanes([0x3C00, 0x4000, 0x3C00, 0x4000, 0x3C00, 0x4000, 0x3C00, 0x4000]);
    assert_eq!(run(0x4EF9_D820, &[[0, 0], x]).v0, lanes([0x3BFC, 0x37FC, 0x3BFC, 0x37FC, 0x3BFC, 0x37FC, 0x3BFC, 0x37FC]));
    // FRSQRTE V0.8H, V1.8H (6EF9D820): lanes 1.0 and 4.0 -> 0x3BFC, 0x37FC.
    let x = lanes([0x3C00, 0x4400, 0x3C00, 0x4400, 0x3C00, 0x4400, 0x3C00, 0x4400]);
    assert_eq!(run(0x6EF9_D820, &[[0, 0], x]).v0, lanes([0x3BFC, 0x37FC, 0x3BFC, 0x37FC, 0x3BFC, 0x37FC, 0x3BFC, 0x37FC]));
    // FRECPS V0.8H, V1.8H, V2.8H (4E423C20): 2 - 1.0*1.5 = 0.5 everywhere.
    assert_eq!(run(0x4E42_3C20, &[[0, 0], ones, lanes([0x3E00; 8])]).v0, lanes([0x3800; 8]));
    // FRSQRTS V0.8H, V1.8H, V2.8H (4EC23C20): (3 - 2.0*1.0)/2 = 0.5 everywhere.
    assert_eq!(run(0x4EC2_3C20, &[[0, 0], lanes([0x4000; 8]), ones]).v0, lanes([0x3800; 8]));
}

#[test]
fn frint_half_vector_returns_the_rounded_lanes() {
    // FRINTN V0.8H, V1.8H (4E798820) and FRINTA V0.8H, V1.8H (6E798820) on
    //   2.5, -2.5, 0.5, 1.5, 3.0, -0.5, 7.0, 1.0
    // ties-to-even: 2.0, -2.0, 0.0, 2.0, 3.0, -0.0, 7.0, 1.0
    // ties-away:    3.0, -3.0, 1.0, 2.0, 3.0, -1.0, 7.0, 1.0
    // V0 starts as all ones: a result that never reached V0 cannot pass.
    let x = lanes([0x4100, 0xC100, 0x3800, 0x3E00, 0x4200, 0xB800, 0x4700, 0x3C00]);
    assert_eq!(
        run(0x4E79_8820, &[[u64::MAX, u64::MAX], x]).v0,
        lanes([0x4000, 0xC000, 0x0000, 0x4000, 0x4200, 0x8000, 0x4700, 0x3C00]),
        "FRINTN .8H"
    );
    assert_eq!(
        run(0x6E79_8820, &[[u64::MAX, u64::MAX], x]).v0,
        lanes([0x4200, 0xC200, 0x3C00, 0x4000, 0x4200, 0xBC00, 0x4700, 0x3C00]),
        "FRINTA .8H"
    );
}
