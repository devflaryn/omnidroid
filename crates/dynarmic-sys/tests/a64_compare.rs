//! **64-bit unsigned compares**: `CMHS`/`CMHI`, scalar `D` and vector `.2D`, executed from guest
//! code.
//!
//! The IR emitter has no 64-bit unsigned compare opcode; it builds one from min/max
//! (`ir_emitter.cpp`: `VectorGreaterEqualUnsigned(e, a, b) = VectorEqual(e, VectorMaxUnsigned(e, a,
//! b), a)` and `VectorGreaterUnsigned(e, a, b) = NOT VectorEqual(e, VectorMinUnsigned(e, a, b), a)`).
//! At `esize == 64` those are `VectorMaxU64` and `VectorMinU64`, which the pin's **arm64** backend
//! implemented as `ASSERT_FALSE("Unimplemented")`. `CMHS_1`/`CMHI_1` (scalar, size must be `11`) and
//! `CMHS_2`/`CMHI_2` with `size = 11` are active decoder entries, so one guest compare terminated the
//! process -- found by `hostile.rs`'s fuzzer at trial 158,510 (`CMHS D18, D23, D24` = `7EF83EF2`).
//! Patch 0006 implements the two.
//!
//! Encodings checked against the LLVM assembler. **Oracle: the ARM ARM** --
//! `CMHS: test_passed = UInt(element1) >= UInt(element2)`, `CMHI: >`, each element all ones when the
//! test passes and zero otherwise. The operands are chosen so that a signed comparison would give the
//! opposite answer (`0x8000_0000_0000_0000` is the largest here unsigned and the smallest signed).

mod harness;

use harness::a64;
use harness::{Vm, VmOptions, HALT_DONE};

const CMHS_D0_D1_D2: u32 = 0x7EE2_3C20;
const CMHI_D0_D1_D2: u32 = 0x7EE2_3420;
const CMHS_2D: u32 = 0x6EE2_3C20;
const CMHI_2D: u32 = 0x6EE2_3420;

const BIG: u64 = 0x8000_0000_0000_0000;
const ONES: u64 = u64::MAX;

fn run(op: u32, v1: [u64; 2], v2: [u64; 2]) -> [u64; 2] {
    let vm = Vm::new(vec![op, a64::svc(0)], VmOptions::default());
    vm.set_vec(0, [0x1111_1111_1111_1111, 0x2222_2222_2222_2222]);
    vm.set_vec(1, v1);
    vm.set_vec(2, v2);
    vm.start(1_000_000);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE, "{op:#010X}");
    vm.vec(0)
}

#[test]
fn scalar_cmhs_and_cmhi_compare_unsigned() {
    // (a, b, a >= b, a > b), unsigned.
    for (a, b, hs, hi) in [
        (BIG, 1, true, true), // 2^63 > 1 unsigned (signed: less)
        (1, BIG, false, false),
        (7, 7, true, false),
        (ONES, 0, true, true),
        (0, ONES, false, false),
    ] {
        let mask = |t: bool| if t { ONES } else { 0 };
        // The scalar form writes D0 and clears V0[127:64].
        assert_eq!(run(CMHS_D0_D1_D2, [a, 5], [b, 6]), [mask(hs), 0], "CMHS {a:#x}, {b:#x}");
        assert_eq!(run(CMHI_D0_D1_D2, [a, 5], [b, 6]), [mask(hi), 0], "CMHI {a:#x}, {b:#x}");
    }
}

#[test]
fn vector_cmhs_and_cmhi_compare_each_lane_unsigned() {
    // Lane 0: 2^63 vs 1 ; lane 1: 3 vs 3.
    assert_eq!(run(CMHS_2D, [BIG, 3], [1, 3]), [ONES, ONES], "CMHS .2D");
    assert_eq!(run(CMHI_2D, [BIG, 3], [1, 3]), [ONES, 0], "CMHI .2D");
    // Swapped: 1 vs 2^63 ; 2 vs 3.
    assert_eq!(run(CMHS_2D, [1, 2], [BIG, 3]), [0, 0], "CMHS .2D swapped");
    assert_eq!(run(CMHI_2D, [1, 2], [BIG, 3]), [0, 0], "CMHI .2D swapped");
}
