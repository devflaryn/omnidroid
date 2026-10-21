//! **What is a thread's own lives in its jit's state, and a reset keeps it** (vendored patch 0112).
//!
//! The arm64 backend's generated code reads at run time, from the jit's state, what it used to bake
//! in as immediates: the callbacks object every callback is called on, the config the non-inline
//! exclusives are given, the thread pointers' slots, and this processor's reservation slots in the
//! monitor -- so that one translated block can later be right for any thread of a process. dynarmic's
//! `Jit::Reset` set that whole state back to zero; with those values in it, the next run would call
//! a callback on a null object and load through null slot pointers.
//!
//! Measured here, for the inline exclusives and for the ones through the monitor's callbacks: the
//! values a jit is made with (`od_jit_thread_values`, arm64 only), that a reset keeps them while the
//! guest's registers go, and that the same translated code run again after the reset still reaches
//! each of them -- the thread pointers' slots, this processor's reservation slot (processor 1 of a
//! monitor of two, so a slot found by the wrong index shows), the exclusive store, and the jit's own
//! callbacks.

mod harness;

use std::ffi::c_void;

use dynarmic_sys::*;
use harness::a64;
use harness::{Vm, VmOptions, HALT_DONE};

/// `MSR TPIDR_EL0, Xt`.
const fn msr_tpidr_el0(rt: u32) -> u32 {
    0xD51B_D040 | rt
}
/// `MRS Xt, TPIDRRO_EL0`.
const fn mrs_tpidrro_el0(rt: u32) -> u32 {
    0xD53B_D060 | rt
}

/// The guest address the exclusives use (a reservation granule's start).
const ADDR: u64 = 0x4000;
/// This test's processor in a monitor of two.
const PROCESSOR: u64 = 1;

/// `MSR TPIDR_EL0, X1 ; MRS X2, TPIDR_EL0 ; MRS X3, TPIDRRO_EL0 ; LDXR X4, [X5] ; SVC #0`, then
/// `ADD X4, X4, #1 ; STXR W6, X4, [X5] ; SVC #0`.
fn program() -> Vec<u32> {
    vec![
        msr_tpidr_el0(1),
        a64::mrs_tpidr_el0(2),
        mrs_tpidrro_el0(3),
        a64::ldxr(4, 5),
        a64::svc(0),
        a64::add_imm(4, 4, 1),
        a64::stxr(6, 4, 5),
        a64::svc(0),
    ]
}

/// The jit's own values as its state holds them; empty where the backend has none (x64).
fn thread_values(vm: &Vm) -> Vec<u64> {
    let mut out = [0u64; OD_THREAD_VALUES as usize];
    // SAFETY: the jit is live and not being reset; `out` holds `OD_THREAD_VALUES` u64s.
    let n = unsafe { od_jit_thread_values(vm.raw(), out.as_mut_ptr(), OD_THREAD_VALUES) };
    out[..n as usize].to_vec()
}

fn layout(monitor: *mut c_void) -> OdMonitorLayout {
    let mut out = OdMonitorLayout::default();
    // SAFETY: a live monitor; `out` is writable.
    unsafe { od_monitor_layout_of(monitor, &mut out) };
    out
}

/// The reservation-address slot of `processor`.
fn reserved(monitor: *mut c_void, processor: u64) -> u64 {
    let l = layout(monitor);
    // SAFETY: the slot is inside the live monitor; it is read while no jit runs.
    unsafe { std::ptr::read_volatile((l.addresses + processor * l.address_stride) as *const u64) }
}

/// [`program`] from its start, the `round`th time: what the guest sees of each per-thread value.
fn run_program(vm: &mut Vm, monitor: *mut c_void, round: u64, what: &str) {
    let tp = 0x7000_0000_1000 + round;
    let tpro = 0x2222_0000 + round;
    vm.set_tpidr_el0(0x1111_0000);
    vm.set_tpidrro_el0(tpro);
    vm.set_reg(1, tp);
    vm.set_reg(5, ADDR);
    vm.with_ctx(|c| {
        c.write_u64(ADDR, 41 + round);
        c.svc.clear();
    });
    vm.start(1_000_000);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE, "{what}: the first SVC");
    assert_eq!(vm.reg(2), tp, "{what}: the guest reads back the TPIDR_EL0 it wrote");
    assert_eq!(vm.tpidr_el0(), tp, "{what}: and the write landed in this jit's slot");
    assert_eq!(vm.reg(3), tpro, "{what}: TPIDRRO_EL0 reads this jit's slot");
    assert_eq!(reserved(monitor, PROCESSOR), ADDR, "{what}: the LDXR reserved in this processor's slot");
    assert_ne!(reserved(monitor, 0), ADDR, "{what}: and not in another processor's");

    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE, "{what}: the second SVC");
    assert_eq!(vm.reg(6), 0, "{what}: the STXR held the reservation and stored");
    assert_eq!(vm.with_ctx(|c| c.read_u64(ADDR)), 42 + round, "{what}: the stored value");
    assert_eq!(vm.with_ctx(|c| c.svc.clone()), [0, 0], "{what}: both SVCs reached this jit's callbacks");
}

#[test]
fn reset_keeps_the_thread_s_own_values() {
    for inline in [true, false] {
        let kind = if inline { "inline exclusives" } else { "exclusives through the monitor" };
        // SAFETY: freed below, after the `Vm`.
        let monitor = unsafe { od_monitor_new(2) };
        assert!(!monitor.is_null());
        let mut vm = Vm::new(
            program(),
            VmOptions {
                fastmem_exclusive: inline,
                cycle_counting: true,
                shared_monitor: monitor as usize,
                processor_id: PROCESSOR as u32,
                ..VmOptions::default()
            },
        );

        let made = thread_values(&vm);
        if cfg!(target_arch = "aarch64") {
            let l = layout(monitor);
            assert_eq!(made.len(), OD_THREAD_VALUES as usize);
            assert!(made[0] != 0 && made[1] != 0, "{kind}: the callbacks object and the config: {made:x?}");
            assert_eq!(
                made[2..],
                [
                    vm.tpidr_el0_ptr(),
                    vm.tpidrro_el0_ptr(),
                    l.addresses + PROCESSOR * l.address_stride,
                    l.values + PROCESSOR * l.value_stride,
                ],
                "{kind}: the thread pointers' slots and this processor's reservation slots"
            );
        }
        run_program(&mut vm, monitor, 0, &format!("{kind}, before the reset"));

        vm.set_reg(1, 0x5555);
        vm.set_sp(0x6666);
        // SAFETY: the jit is live, on this thread, and not executing.
        assert_eq!(unsafe { od_jit_reset(vm.raw()) }, 1);
        assert_eq!((vm.reg(1), vm.sp(), vm.pc()), (0, 0, 0), "{kind}: the guest's state went");
        assert_eq!(thread_values(&vm), made, "{kind}: the reset kept the thread's own values");
        assert_eq!(vm.effective_config().tpidr_el0_ptr, vm.tpidr_el0_ptr(), "{kind}: the slot generated code uses");

        // The same translated code, run again.
        run_program(&mut vm, monitor, 1, &format!("{kind}, after the reset"));
        drop(vm);
        // SAFETY: the only jit on it is gone.
        unsafe { od_monitor_free(monitor) };
    }
}
