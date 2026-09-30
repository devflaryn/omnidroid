//! **The unsafe floating-point flags, switched while a process runs** (patch 0034, x64): a block
//! translated after `set_live_fp_optimizations` and a cache clear is emitted with them; before, and
//! after switching back, with the architecture's exact results.
//!
//! The witness is `FRSQRTE`: exact, it is the architecture's 8-bit estimate (a software model the
//! JIT calls), so its low 15 mantissa bits are zero; under `Unsafe_ReducedErrorFP` it is the host's
//! `rsqrtss`, whose 12-bit estimate carries more bits.
#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::set_live_fp_optimizations;
use omni_cpu::{ExitReason, GuestCpu, RunLimit};

/// `FMOV Sd, Wn`.
const fn fmov_s_from_w(rd: u32, rn: u32) -> u32 {
    0x1E27_0000 | (rn << 5) | rd
}
/// `FMOV Wd, Sn`.
const fn fmov_w_from_s(rd: u32, rn: u32) -> u32 {
    0x1E26_0000 | (rn << 5) | rd
}
/// `FRSQRTE Sd, Sn` (scalar, single precision).
const fn frsqrte_s(rd: u32, rn: u32) -> u32 {
    0x7EA1_D800 | (rn << 5) | rd
}

const REDUCED_ERROR_FP: u32 = 0x0002_0000;

/// The estimates of 1/sqrt(v) for a few inputs, as `f32` bit patterns, as the guest computes them.
fn estimates(guest: &Guest, function: omni_cpu::GuestAddr) -> Vec<u32> {
    let (mut cpu, sentinel) = guest.thread();
    [3.0f32, 5.0, 7.0, 10.0, 0.3]
        .iter()
        .map(|v| {
            cpu.set_x(x(1), u64::from(v.to_bits()));
            cpu.set_x(x(30), sentinel as u64);
            assert_eq!(cpu.run(function, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
            cpu.x(x(0)) as u32
        })
        .collect()
}

#[test]
fn a_switch_and_a_clear_change_what_the_next_translation_computes() {
    let guest = Guest::new();
    let function = guest.load(&[fmov_s_from_w(1, 1), frsqrte_s(0, 1), fmov_w_from_s(0, 0), ret(30)]);

    assert_eq!(set_live_fp_optimizations(0), 0);
    let exact = estimates(&guest, function);
    for (bits, v) in exact.iter().zip([3.0f32, 5.0, 7.0, 10.0, 0.3]) {
        assert_eq!(bits & 0x7fff, 0, "the architecture's estimate of 1/sqrt({v}) has 8 bits: {bits:#x}");
        assert!((f32::from_bits(*bits) - 1.0 / v.sqrt()).abs() < 1.0 / v.sqrt() / 128.0);
    }

    // Switched, but the old translation is still what runs.
    assert_eq!(set_live_fp_optimizations(0xffff_ffff), 0x000f_0000, "only the unsafe floating-point bits are kept");
    assert_eq!(estimates(&guest, function), exact, "a block translated before the switch keeps its flags");

    // Cleared: translated again, with the host's estimate.
    assert_eq!(set_live_fp_optimizations(REDUCED_ERROR_FP), REDUCED_ERROR_FP);
    guest.backend.clear_code_cache();
    let host = estimates(&guest, function);
    assert_ne!(host, exact, "under Unsafe_ReducedErrorFP the estimate is the host's");
    for (bits, v) in host.iter().zip([3.0f32, 5.0, 7.0, 10.0, 0.3]) {
        assert!((f32::from_bits(*bits) - 1.0 / v.sqrt()).abs() < 1.0 / v.sqrt() / 1024.0, "{v}: {bits:#x}");
    }

    // And back.
    assert_eq!(set_live_fp_optimizations(0), 0);
    guest.backend.clear_code_cache();
    assert_eq!(estimates(&guest, function), exact, "switched off and cleared, the exact estimate again");
}
