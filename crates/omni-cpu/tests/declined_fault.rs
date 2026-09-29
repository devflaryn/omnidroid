//! `DynarmicOptions::recompile_on_declined_fault`: a fault the guest meant -- ART's implicit null
//! check loads through a null object and turns the `SIGSEGV` into a `NullPointerException` --
//! becomes a typed exit either way. Off (the Linux personality), the instruction's next, valid
//! execution is back on the direct path; on (the default), arm64's per-thread cache has moved it to
//! the callback path for good, which the per-slice invariant then kills.
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::DynarmicOptions;
use omni_cpu::{ExitReason, GuestCpu, RunLimit};

/// `ldr x2, [x0]; ret`: run once through null, then through the data region.
fn null_then_valid(recompile: bool) -> (ExitReason, Result<ExitReason, omni_cpu::CpuError>, u64) {
    let guest = Guest::with_options(DynarmicOptions { recompile_on_declined_fault: recompile, ..DynarmicOptions::default() });
    let entry = guest.load(&[ldr_imm(2, 0, 0), ret(30)]);
    let (mut cpu, sentinel) = guest.thread();
    cpu.set_x(x(0), 0);
    let null = cpu.run(entry, RunLimit::Unlimited).expect("the null load exits");
    guest.write_u64(guest.data, 0x5A5A);
    cpu.set_x(x(0), guest.data as u64);
    cpu.set_x(x(30), sentinel as u64);
    let before = cpu.slow_path_entries();
    let valid = cpu.run(entry, RunLimit::Unlimited);
    let after = cpu.slow_path_entries() - before;
    if valid.is_ok() {
        assert_eq!(cpu.x(x(2)), 0x5A5A, "the valid load read the data");
    }
    (null, valid, after)
}

#[test]
fn off_a_null_load_is_a_fault_and_the_same_load_then_stays_on_the_direct_path() {
    let (null, valid, slow) = null_then_valid(false);
    assert!(matches!(null, ExitReason::MemoryFault { address: 0, .. }), "{null}");
    assert!(matches!(valid, Ok(ExitReason::Returned { .. })), "{valid:?}");
    assert_eq!(slow, 0, "the instruction is back on the direct path");
}

#[test]
fn on_arm64_moves_the_instruction_to_the_callback_path_for_good() {
    let (null, valid, slow) = null_then_valid(true);
    assert!(matches!(null, ExitReason::MemoryFault { address: 0, .. }), "{null}");
    if cfg!(target_arch = "aarch64") {
        // Per-thread translation caches: the block was rebuilt with the load on the callback path,
        // and the invariant refuses the slice that used it.
        assert!(
            matches!(valid, Err(omni_cpu::CpuError::DegradedMemoryPath { .. })) || slow > 0,
            "recompiled onto the callback path: {valid:?}, {slow} slow-path entries"
        );
    }
}
