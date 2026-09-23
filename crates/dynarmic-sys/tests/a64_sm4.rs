//! **SM4** (`SM4E`, `SM4EKEY`, FEAT_SM4), executed from guest code on the standard's own test
//! vector.
//!
//! The A64 frontend translates both instructions (`simd_sha512.cpp`, `SM4Hash`) into IR that looks
//! bytes up with `SM4AccessSubstitutionBox`, which the pin's **arm64** backend implemented as
//! `ASSERT_FALSE("Unimplemented")` (`emit_arm64_cryptography.cpp`): the first `SM4E` terminated the
//! process. Patch 0005 implements it.
//!
//! **Oracle: the SM4 specification's example** (GB/T 32907-2016, Appendix A, example 1): key and
//! plaintext `01234567 89abcdef fedcba98 76543210` encrypt to `681edf34 d206965e 86b3e94f 536e4246`.
//! Thirty-two rounds through eight `SM4EKEY` and eight `SM4E` cannot land on that by accident, and
//! nothing in it comes from dynarmic. The register conventions are the ARM ARM's: `SM4EKEY Vd, Vn, Vm`
//! takes the four previous key words `K[i..i+4]` as elements 0-3 of `Vn` and `CK[i..i+4]` in `Vm`, and
//! yields `K[i+4..i+8]` = `rk[i..i+4]`; `SM4E Vd, Vn` takes the state `X[i..i+4]` in `Vd` and
//! `rk[i..i+4]` in `Vn` and leaves `X[i+4..i+8]`. After 32 rounds `Vd` holds `X32..X35`, and the
//! ciphertext is those four words in reverse order.

mod harness;

use harness::a64;
use harness::{Vm, VmOptions, HALT_DONE};

/// `SM4EKEY Vd.4S, Vn.4S, Vm.4S` — `11001110011 Rm:5 110010 Rn:5 Rd:5` (LLVM: `sm4ekey v0.4s,
/// v1.4s, v2.4s` = `ce62c820`).
const fn sm4ekey(rd: u32, rn: u32, rm: u32) -> u32 {
    0xCE60_C800 | (rm << 16) | (rn << 5) | rd
}

/// `SM4E Vd.4S, Vn.4S` — `1100111011000000100001 Rn:5 Rd:5` (LLVM: `sm4e v0.4s, v1.4s` =
/// `cec08420`).
const fn sm4e(rd: u32, rn: u32) -> u32 {
    0xCEC0_8400 | (rn << 5) | rd
}

/// Four 32-bit lanes, element 0 first, as the two `u64` halves of a `V` register.
fn v4s(w: [u32; 4]) -> [u64; 2] {
    [u64::from(w[0]) | (u64::from(w[1]) << 32), u64::from(w[2]) | (u64::from(w[3]) << 32)]
}

#[test]
fn sm4_encrypts_the_standard_s_example() {
    assert_eq!(sm4ekey(0, 1, 2), 0xCE62_C820);
    assert_eq!(sm4e(0, 1), 0xCEC0_8420);

    // The specification's constants. FK is given; CK[i] is the word of bytes (4i+j)*7 mod 256,
    // j = 0..3, most significant first.
    const FK: [u32; 4] = [0xA3B1_BAC6, 0x56AA_3350, 0x677D_9197, 0xB270_22DC];
    let ck: Vec<u32> = (0..32u32)
        .map(|i| (0..4u32).fold(0u32, |w, j| (w << 8) | (((4 * i + j) * 7) % 256)))
        .collect();
    assert_eq!(&ck[..4], &[0x0007_0E15, 0x1C23_2A31, 0x383F_464D, 0x545B_6269], "CK as tabulated");

    const MK: [u32; 4] = [0x0123_4567, 0x89AB_CDEF, 0xFEDC_BA98, 0x7654_3210];
    const PLAIN: [u32; 4] = MK;
    const CIPHER: [u32; 4] = [0x681E_DF34, 0xD206_965E, 0x86B3_E94F, 0x536E_4246];

    // V0 = K[0..4] = MK ^ FK ; V16..V23 = CK in fours ; V30 = the plaintext.
    // SM4EKEY V1, V0, V16 ; SM4EKEY V2, V1, V17 ; ... ; SM4EKEY V8, V7, V23   (rk[0..32] in V1..V8)
    // SM4E V30, V1 ; SM4E V30, V2 ; ... ; SM4E V30, V8 ; SVC #0
    let mut code = Vec::new();
    for i in 0..8 {
        code.push(sm4ekey(i + 1, i, 16 + i));
    }
    for i in 0..8 {
        code.push(sm4e(30, i + 1));
    }
    code.push(a64::svc(0));

    let vm = Vm::new(code, VmOptions::default());
    vm.set_vec(0, v4s([MK[0] ^ FK[0], MK[1] ^ FK[1], MK[2] ^ FK[2], MK[3] ^ FK[3]]));
    for i in 0..8 {
        vm.set_vec(16 + i as u32, v4s([ck[4 * i], ck[4 * i + 1], ck[4 * i + 2], ck[4 * i + 3]]));
    }
    vm.set_vec(30, v4s(PLAIN));
    vm.start(1_000_000);
    let hr = vm.run_to_completion(16);
    assert_eq!(hr & HALT_DONE, HALT_DONE, "the guest did not reach its SVC (halt {hr:#010X})");

    // X32..X35 in elements 0..3, i.e. the ciphertext reversed.
    assert_eq!(
        vm.vec(30),
        v4s([CIPHER[3], CIPHER[2], CIPHER[1], CIPHER[0]]),
        "SM4(01234567 89abcdef fedcba98 76543210) under the same key"
    );
}
