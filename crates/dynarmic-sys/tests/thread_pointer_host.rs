//! **The guest's thread pointers are slots, never the host's registers.**
//!
//! On an arm64 host the guest's `TPIDR_EL0`/`TPIDRRO_EL0` and the host's are the same architectural
//! registers, and macOS keeps its own thread-local storage behind `TPIDRRO_EL0` (read-only at EL0).
//! If generated code ever read or wrote the host registers for the guest's, every host
//! `thread_local!` -- the allocator's, Rust's, dynarmic's own -- would break the first time guest
//! code set its thread pointer (D13: bionic does, before any other guest code runs).
//!
//! What the arm64 backend does (`emit_arm64_a64.cpp`, `A64GetTPIDR`/`A64SetTPIDR`/`A64GetTPIDRRO`):
//! load and store through the `tpidr_el0`/`tpidrro_el0` **pointers** in the config -- memory slots --
//! and no emitter names the system registers. A guest `MSR TPIDRRO_EL0` is UNDEFINED at EL0 (ARM
//! ARM) and the frontend does not translate it: it reaches the interpreter fallback.
//!
//! Measured here: the host's `TPIDRRO_EL0` -- the one macOS keeps its TLS behind -- read before a
//! run, inside a callback in the middle of it, and after it, never moves, while the guest reads and
//! writes its own values. The host's `TPIDR_EL0` is **not** stable and is not asserted to be: on
//! macOS the kernel keeps the current CPU number there and rewrites it when the thread migrates
//! (MEASURED on Apple M1: 4100 before a run and 4102 inside it; 3 before and 0 after another). What
//! is asserted of it is that it never holds a value the guest wrote.

#![cfg(target_arch = "aarch64")]

mod harness;

use harness::a64;
use harness::{Vm, VmOptions, HALT_DONE};

fn host_thread_pointers() -> (u64, u64) {
    let (tp, tpro): (u64, u64);
    // SAFETY: both registers are readable at EL0; `mrs` touches no memory.
    unsafe { core::arch::asm!("mrs {}, tpidr_el0", "mrs {}, tpidrro_el0", out(reg) tp, out(reg) tpro, options(nomem, nostack)) };
    (tp, tpro)
}

/// `MSR TPIDR_EL0, Xt`.
const fn msr_tpidr_el0(rt: u32) -> u32 {
    0xD51B_D040 | rt
}
/// `MRS Xt, TPIDRRO_EL0`.
const fn mrs_tpidrro_el0(rt: u32) -> u32 {
    0xD53B_D060 | rt
}
/// `MSR TPIDRRO_EL0, Xt` -- UNDEFINED at EL0.
const fn msr_tpidrro_el0(rt: u32) -> u32 {
    0xD51B_D060 | rt
}

thread_local! {
    static CANARY: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[test]
fn guest_thread_pointer_reads_and_writes_never_touch_the_host_registers() {
    assert_eq!(msr_tpidr_el0(1), 0xD51B_D041);
    assert_eq!(mrs_tpidrro_el0(3), 0xD53B_D063);
    CANARY.with(|c| c.set(0x5EED));
    let before = host_thread_pointers();

    // MSR TPIDR_EL0, X1 ; MRS X2, TPIDR_EL0 ; MRS X3, TPIDRRO_EL0 ; SVC #0
    let code = vec![msr_tpidr_el0(1), a64::mrs_tpidr_el0(2), mrs_tpidrro_el0(3), a64::svc(0)];
    let mut vm = Vm::new(code, VmOptions::default());
    vm.set_tpidr_el0(0x1111_0000);
    vm.set_tpidrro_el0(0x2222_0000);
    vm.set_reg(1, 0xDEAD_BEEF_0000);
    vm.start(1_000_000);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);

    assert_eq!(vm.reg(2), 0xDEAD_BEEF_0000, "the guest reads back what it wrote");
    assert_eq!(vm.tpidr_el0(), 0xDEAD_BEEF_0000, "and the write landed in the slot");
    assert_eq!(vm.reg(3), 0x2222_0000, "TPIDRRO_EL0 reads the slot");
    let inside = vm.with_ctx(|c| c.host_thread_pointers_in_svc);
    let after = host_thread_pointers();
    assert_eq!(inside.1, before.1, "the host's TPIDRRO_EL0 moved while guest code ran");
    assert_eq!(after.1, before.1, "the host's TPIDRRO_EL0 moved across the run");
    for (when, tp) in [("inside", inside.0), ("after", after.0)] {
        assert!(
            tp != 0xDEAD_BEEF_0000 && tp != 0x1111_0000,
            "{when} the run the host's TPIDR_EL0 holds a guest value: {tp:#x}"
        );
    }
    assert_eq!(CANARY.with(|c| c.get()), 0x5EED, "host thread-local storage still works");
    assert_ne!(before.1, 0x2222_0000);
    assert_ne!(before.0, 0xDEAD_BEEF_0000);
}

#[test]
fn a_guest_write_to_tpidrro_el0_is_refused_and_changes_nothing() {
    let before = host_thread_pointers();
    // MSR TPIDRRO_EL0, X1 ; SVC #0
    let mut vm = Vm::new(vec![msr_tpidrro_el0(1), a64::svc(0)], VmOptions::default());
    vm.set_tpidrro_el0(0x2222_0000);
    vm.set_reg(1, 0x3333_0000);
    vm.start(1_000_000);
    let hr = vm.run();
    assert_eq!(hr & HALT_DONE, 0, "the guest must not get past an EL0 write to TPIDRRO_EL0");
    let fallbacks = vm.with_ctx(|c| c.fallbacks.clone());
    assert_eq!(fallbacks, [(harness::CODE_BASE, 1)], "it goes to the interpreter fallback");
    assert_eq!(vm.tpidrro_el0(), 0x2222_0000, "the slot is unchanged");
    let after = host_thread_pointers();
    assert_eq!(after.1, before.1, "the host's TPIDRRO_EL0 is unchanged");
    assert_ne!(after.0, 0x3333_0000, "and the host's TPIDR_EL0 did not take the guest's value");
}
