//! **Scalar saturating arithmetic** — `SQADD`, `UQADD`, `SQSUB`, `UQSUB` at all four element sizes
//! and `SQDMULH` at H and S — executed from guest code, with the result and `FPSR.QC` checked.
//!
//! These reach IR opcodes (`SignedSaturatedAdd8` … `UnsignedSaturatedSub64`,
//! `SignedSaturatedDoublingMultiplyReturnHigh16/32`) that the pin's **arm64** backend implemented as
//! `ASSERT_FALSE("Unimplemented")` (`emit_arm64_saturation.cpp`): one guest instruction terminated
//! the process. Carried patch 0003 implements them. The frontend reaches them from
//! `simd_scalar_three_same.cpp` (`SQADD_1`, `UQADD_1`, `SQSUB_1`, `UQSUB_1`, `SQDMULH_vec_1`) with no
//! size guard for the four add/sub forms.
//!
//! **Oracle: the ARM ARM pseudocode, by hand** (VERIFICATION entry 7) — never another
//! implementation. For the add/sub forms, `result = SatQ(Int(a) op Int(b), esize, unsigned)` and
//! `FPSR.QC` is set when `SatQ` saturated (`shared/functions/integer/SatQ`). For `SQDMULH`,
//! `product = (2 * SInt(a) * SInt(b)) >> esize` (no rounding), then `SignedSatQ(product, esize)`.
//! A scalar write to `V[d]` clears every bit above the element, which is asserted too. Every
//! expected value below is worked in the comment beside it.

mod harness;

use harness::a64;
use harness::{Vm, VmOptions, HALT_DONE};

/// `FPSR.QC`, the cumulative saturation bit.
const QC: u64 = 1 << 27;

/// Runs `op` (which reads `V1`, `V2` and writes `V0`) with the two operands in the low element,
/// every other bit of `V0` set beforehand, and returns (`V0` low 64, `V0` high 64, `FPSR`).
///
/// `op; MRS X0, FPSR; SVC #0`.
fn run_op(op: u32, a: u64, b: u64) -> (u64, u64, u64) {
    let code = vec![op, a64::mrs_fpsr(0), a64::svc(0)];
    let vm = Vm::new(code, VmOptions::default());
    vm.set_vec(0, [u64::MAX, u64::MAX]);
    vm.set_vec(1, [a, 0xDEAD_BEEF_DEAD_BEEF]);
    vm.set_vec(2, [b, 0xFEED_FACE_FEED_FACE]);
    vm.start(1_000_000);
    let hr = vm.run_to_completion(16);
    assert_eq!(hr & HALT_DONE, HALT_DONE, "{op:#010X}: guest did not reach SVC (halt {hr:#010X})");
    let v0 = vm.vec(0);
    (v0[0], v0[1], vm.reg(0))
}

/// One case: `op` on (`a`, `b`) must give `want` in the element and `qc` in `FPSR.QC`, with the
/// rest of `V0` cleared.
fn check(name: &str, op: u32, a: u64, b: u64, want: u64, qc: bool) {
    let (lo, hi, fpsr) = run_op(op, a, b);
    assert_eq!(lo, want, "{name}: {a:#x}, {b:#x} -> {lo:#x}, want {want:#x}");
    assert_eq!(hi, 0, "{name}: the scalar write must clear V0[127:64]");
    assert_eq!(fpsr & QC != 0, qc, "{name}: FPSR.QC = {} (FPSR {fpsr:#x})", fpsr & QC != 0);
}

#[test]
fn the_encoders_match_the_arm_arm() {
    // `01 U 11110 size 1 Rm opcode 1 Rn Rd`, Rd=0 Rn=1 Rm=2.
    assert_eq!(a64::sqadd_scalar(0, 0, 1, 2), 0x5E22_0C20); // SQADD B0, B1, B2
    assert_eq!(a64::sqadd_scalar(3, 0, 1, 2), 0x5EE2_0C20); // SQADD D0, D1, D2
    assert_eq!(a64::uqadd_scalar(1, 0, 1, 2), 0x7E62_0C20); // UQADD H0, H1, H2
    assert_eq!(a64::sqsub_scalar(2, 0, 1, 2), 0x5EA2_2C20); // SQSUB S0, S1, S2
    assert_eq!(a64::uqsub_scalar(0, 0, 1, 2), 0x7E22_2C20); // UQSUB B0, B1, B2
    assert_eq!(a64::sqdmulh_scalar(1, 0, 1, 2), 0x5E62_B420); // SQDMULH H0, H1, H2
    assert_eq!(a64::sqdmulh_scalar(2, 0, 1, 2), 0x5EA2_B420); // SQDMULH S0, S1, S2
    assert_eq!(a64::mrs_fpsr(0), 0xD53B_4420); // MRS X0, FPSR
}

#[test]
fn sqadd_saturates_at_each_signed_bound() {
    for (size, bits) in [(0u32, 8u32), (1, 16), (2, 32), (3, 64)] {
        let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
        let max = mask >> 1; //            0x7F...
        let min = max + 1; //              0x80...
        let op = a64::sqadd_scalar(size, 0, 1, 2);
        // max + 1 = 2^(n-1), above the range: saturates to max.
        check(&format!("SQADD.{bits} max+1"), op, max, 1, max, true);
        // min + (-1) = -2^(n-1) - 1, below: saturates to min.
        check(&format!("SQADD.{bits} min-1"), op, min, mask, min, true);
        // 0x10 + 0x20 = 0x30, in range.
        check(&format!("SQADD.{bits} in range"), op, 0x10, 0x20, 0x30, false);
        // -1 + -1 = -2, in range, all ones minus one.
        check(&format!("SQADD.{bits} -1-1"), op, mask, mask, mask - 1, false);
    }
}

#[test]
fn uqadd_saturates_at_the_unsigned_maximum() {
    for (size, bits) in [(0u32, 8u32), (1, 16), (2, 32), (3, 64)] {
        let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
        let op = a64::uqadd_scalar(size, 0, 1, 2);
        // (2^n - 1) + 1 = 2^n, above: saturates to 2^n - 1.
        check(&format!("UQADD.{bits} max+1"), op, mask, 1, mask, true);
        // (2^n - 1) + (2^n - 1) saturates too.
        check(&format!("UQADD.{bits} max+max"), op, mask, mask, mask, true);
        check(&format!("UQADD.{bits} in range"), op, 0x10, 0x20, 0x30, false);
    }
}

#[test]
fn sqsub_saturates_at_each_signed_bound() {
    for (size, bits) in [(0u32, 8u32), (1, 16), (2, 32), (3, 64)] {
        let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
        let max = mask >> 1;
        let min = max + 1;
        let op = a64::sqsub_scalar(size, 0, 1, 2);
        // min - 1 = -2^(n-1) - 1, below: min.
        check(&format!("SQSUB.{bits} min-1"), op, min, 1, min, true);
        // max - (-1) = 2^(n-1), above: max.
        check(&format!("SQSUB.{bits} max+1"), op, max, mask, max, true);
        // 0x10 - 0x20 = -0x10, in range: two's complement in n bits.
        check(&format!("SQSUB.{bits} in range"), op, 0x10, 0x20, mask - 0xF, false);
    }
}

#[test]
fn uqsub_saturates_at_zero() {
    for (size, bits) in [(0u32, 8u32), (1, 16), (2, 32), (3, 64)] {
        let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
        let op = a64::uqsub_scalar(size, 0, 1, 2);
        // 0 - 1 = -1, below: 0.
        check(&format!("UQSUB.{bits} 0-1"), op, 0, 1, 0, true);
        // 0x10 - (2^n - 1) is negative: 0.
        check(&format!("UQSUB.{bits} small-max"), op, 0x10, mask, 0, true);
        check(&format!("UQSUB.{bits} in range"), op, 0x30, 0x10, 0x20, false);
    }
}

#[test]
fn sqdmulh_doubles_takes_the_high_half_and_saturates_only_minus_one_squared() {
    // H: (2 * -32768 * -32768) >> 16 = 2^31 >> 16 = 32768, one past 0x7FFF: saturates.
    check("SQDMULH.16 min*min", a64::sqdmulh_scalar(1, 0, 1, 2), 0x8000, 0x8000, 0x7FFF, true);
    // H: (2 * 16384 * 16384) >> 16 = 2^29 >> 16 = 0x2000.
    check("SQDMULH.16 half*half", a64::sqdmulh_scalar(1, 0, 1, 2), 0x4000, 0x4000, 0x2000, false);
    // H: (2 * -16384 * 16384) >> 16 = -2^29 >> 16 = -0x2000 = 0xE000.
    check("SQDMULH.16 -half*half", a64::sqdmulh_scalar(1, 0, 1, 2), 0xC000, 0x4000, 0xE000, false);
    // H: (2 * 3 * 5) >> 16 = 30 >> 16 = 0 -- truncation, not rounding (SQRDMULH would round).
    check("SQDMULH.16 small", a64::sqdmulh_scalar(1, 0, 1, 2), 3, 5, 0, false);
    // H: (2 * -1 * 1) >> 16 = -2 >> 16 = -1 (arithmetic shift rounds towards -inf) = 0xFFFF.
    check("SQDMULH.16 -1*1", a64::sqdmulh_scalar(1, 0, 1, 2), 0xFFFF, 1, 0xFFFF, false);
    // S: (2 * -2^31 * -2^31) >> 32 = 2^63 >> 32 = 2^31, one past 0x7FFFFFFF: saturates.
    check(
        "SQDMULH.32 min*min",
        a64::sqdmulh_scalar(2, 0, 1, 2),
        0x8000_0000,
        0x8000_0000,
        0x7FFF_FFFF,
        true,
    );
    // S: (2 * 2^30 * 2^30) >> 32 = 2^61 >> 32 = 2^29.
    check(
        "SQDMULH.32 half*half",
        a64::sqdmulh_scalar(2, 0, 1, 2),
        0x4000_0000,
        0x4000_0000,
        0x2000_0000,
        false,
    );
    // S: (2 * -2^30 * 3) >> 32 = -3 * 2^31 >> 32 = -1.5 -> floor -2 = 0xFFFFFFFE.
    check(
        "SQDMULH.32 -half*3",
        a64::sqdmulh_scalar(2, 0, 1, 2),
        0xC000_0000,
        3,
        0xFFFF_FFFE,
        false,
    );
}

#[test]
fn qc_is_sticky_and_a_later_unsaturated_op_does_not_clear_it() {
    // SQADD B0, B1, B2 (saturates) ; UQADD B3, B4, B5 (does not) ; MRS X0, FPSR ; SVC #0
    let code = vec![
        a64::sqadd_scalar(0, 0, 1, 2),
        a64::uqadd_scalar(0, 3, 4, 5),
        a64::mrs_fpsr(0),
        a64::svc(0),
    ];
    let vm = Vm::new(code, VmOptions::default());
    vm.set_vec(1, [0x7F, 0]);
    vm.set_vec(2, [0x01, 0]);
    vm.set_vec(4, [0x01, 0]);
    vm.set_vec(5, [0x02, 0]);
    vm.start(1_000_000);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.vec(0)[0], 0x7F);
    assert_eq!(vm.vec(3)[0], 0x03);
    assert_ne!(vm.reg(0) & QC, 0, "QC is cumulative: FPSR {:#x}", vm.reg(0));
}
