//! A guest `SVC #0` served inside the run loop by a registered handler (sub-project A), and a
//! thread whose thread pointer the guest itself manages.
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::{
    ExitReason, GuestAddressSpace, GuestCpu, GuestCpuBackend, GuestThreadConfig, RunLimit,
    ThunkCall, ThunkContext,
};

/// `x0 = x8 * 1000 + x0 + context`: proves the handler saw the number, an argument and its context.
fn serve(call: &mut ThunkCall<'_>) {
    let value = call.x(8) * 1000 + call.x(0) + call.context().0 as u64;
    call.set_x(0, value);
}

fn defer(call: &mut ThunkCall<'_>) {
    call.defer_to_caller();
}

#[test]
fn a_guest_syscall_is_served_in_the_loop_and_execution_continues_after_it() {
    let guest = Guest::new();
    let program = [
        movz(8, 172, 0), // x8 = 172 (getpid)
        movz(0, 5, 0),   // x0 = 5
        svc(0),
        add_imm(1, 0, 1), // x1 = x0 + 1: runs only if execution resumed after the SVC
        ret(30),
    ];
    let entry = guest.load(&program);
    let (mut cpu, sentinel) = guest.thread();
    cpu.set_svc_handler(serve, ThunkContext(7)).expect("a syscall handler");

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");

    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.x(x(0)), 172 * 1000 + 5 + 7);
    assert_eq!(cpu.x(x(1)), 172 * 1000 + 5 + 7 + 1);
}

#[test]
fn a_deferred_syscall_stops_the_run_at_the_svc() {
    let guest = Guest::new();
    let entry = guest.load(&[movz(8, 93, 0), svc(0), ret(30)]);
    let (mut cpu, _sentinel) = guest.thread();
    cpu.set_svc_handler(defer, ThunkContext(0)).expect("a syscall handler");

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");

    assert_eq!(
        exit,
        ExitReason::UnsupportedInstruction { pc: entry + 4, encoding: svc(0) },
        "{exit}"
    );
}

#[test]
fn without_a_handler_a_guest_syscall_still_stops_as_before() {
    let guest = Guest::new();
    let entry = guest.load(&[svc(0), ret(30)]);
    let (mut cpu, _sentinel) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");
    assert_eq!(exit, ExitReason::UnsupportedInstruction { pc: entry, encoding: svc(0) }, "{exit}");
}

#[test]
fn a_guest_managed_thread_starts_with_a_null_thread_pointer_and_sets_its_own() {
    let guest = Guest::new();
    let program = [
        mrs_tpidr_el0(0), // x0 = the thread pointer the thread started with
        movz(1, 0x1234, 0),
        msr_tpidr_el0(1), // the guest sets its own
        mrs_tpidr_el0(2),
        ret(30),
    ];
    let entry = guest.load(&program);
    let config = GuestThreadConfig::guest_managed(
        GuestAddressSpace::of(&guest.space).expect("the space's extent"),
    );
    let mut cpu = guest.backend.create_thread(config).expect("a guest-managed thread");
    let sentinel = guest.code + harness::CODE_BYTES - 4;
    cpu.set_return_sentinel(sentinel).expect("arm the sentinel");
    cpu.set_x(x(30), sentinel as u64);

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");

    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.x(x(0)), 0, "the kernel starts a thread with TPIDR_EL0 = 0");
    assert_eq!(cpu.x(x(2)), 0x1234);
    assert_eq!(cpu.tpidr_el0(), 0x1234);
}
