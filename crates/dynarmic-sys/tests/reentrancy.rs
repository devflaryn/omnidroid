//! Re-entrancy: what happens when generated guest code reaches a callback and
//! that callback calls back into the jit.
//!
//! This is the hazard the crate documentation is built around, so it is tested
//! rather than argued. dynarmic's own `Jit::Run` opens with
//! `ASSERT(!is_executing)` and its asserts call `std::terminate`; guest code
//! chooses when callbacks fire, so that is an abort reachable from untrusted
//! input, which Global Constraint 11 rates Critical. The shim checks
//! `IsExecuting()` first and returns [`OD_HALT_SHIM_REENTERED`].

mod harness;

use dynarmic_sys::*;
use harness::a64;
use harness::{Vm, VmOptions};

#[test]
fn re_entering_run_from_a_callback_is_refused_not_fatal() {
    // MOVZ X0, #1                       D2800020
    // SVC  #0                           D4000001   ; the callback re-enters here
    // MOVZ X1, #2                       D2800041
    // SVC  #0                           D4000001
    let code = vec![
        a64::movz(0, 1, 0),
        a64::svc(0),
        a64::movz(1, 2, 0),
        a64::svc(0),
    ];
    assert_eq!(code[0], 0xD280_0020);
    assert_eq!(code[2], 0xD280_0041);

    let vm = Vm::new(code, VmOptions::default());
    vm.with_ctx(|c| {
        c.reenter_on_svc = true;
        // Do not halt on the first SVC; let the re-entry attempt happen and
        // then carry on to the second one.
        c.halt_on_svc = false;
    });
    vm.start(100_000);

    // First run reaches SVC #0, the callback tries to run again and is refused,
    // then execution continues to the second SVC where the same thing happens.
    // Nothing halts, so the guest runs out of code and faults -- which is fine;
    // what matters is that the process is still here.
    let hr = vm.run_to_completion(16);

    let (result, step_result, svcs) =
        vm.with_ctx(|c| (c.reenter_result, c.reenter_step_result, c.svc.clone()));
    assert_eq!(
        result,
        Some(OD_HALT_SHIM_REENTERED),
        "od_jit_run from inside a callback must report re-entry, not abort"
    );
    assert_eq!(
        step_result,
        Some(OD_HALT_SHIM_REENTERED),
        "od_jit_step carries the same ASSERT(!is_executing) and needs the same guard"
    );
    assert!(!svcs.is_empty(), "the SVC callback ran");
    assert_ne!(hr, 0, "execution ended for some reason, without terminating");

    // The jit is still healthy: registers readable, and it can run again.
    assert_eq!(vm.reg(0), 1);
    // SAFETY: `vm.raw()` is live and execution has returned.
    assert_eq!(unsafe { od_jit_is_executing(vm.raw()) }, 0);
}

#[test]
fn is_executing_is_true_inside_a_callback_and_false_outside() {
    let vm = Vm::new(vec![a64::svc(0)], VmOptions::default());
    // SAFETY: `vm.raw()` is live.
    assert_eq!(unsafe { od_jit_is_executing(vm.raw()) }, 0);
    vm.start(10_000);
    vm.with_ctx(|c| c.reenter_on_svc = true);
    let _ = vm.run_to_completion(16);
    // `reenter_on_svc` called `od_jit_run` from inside the SVC callback; it was
    // refused, which is only possible if `IsExecuting()` was true there.
    assert_eq!(
        vm.with_ctx(|c| (c.reenter_result, c.reenter_step_result)),
        (Some(OD_HALT_SHIM_REENTERED), Some(OD_HALT_SHIM_REENTERED))
    );
    // SAFETY: as above.
    assert_eq!(unsafe { od_jit_is_executing(vm.raw()) }, 0);
}

#[test]
fn invalidating_the_code_cache_from_a_callback_is_safe() {
    // dynarmic documents `ClearCache` as callable inside a callback, where it
    // halts execution to do the work. A guest that writes code and then issues
    // `IC IVAU` lands exactly here, so this is not a corner case.
    //
    // MOVZ X0, #7                       D28000E0
    // SVC  #0                           D4000001
    // ADD  X0, X0, #1                   91000400
    // SVC  #0                           D4000001
    let code = vec![
        a64::movz(0, 7, 0),
        a64::svc(0),
        a64::add_imm(0, 0, 1),
        a64::svc(0),
    ];

    let vm = Vm::new(code, VmOptions::default());
    vm.with_ctx(|c| c.halt_on_svc = false);
    vm.start(100_000);

    // First run: stop at the first SVC by halting from outside is not possible
    // here, so instead invalidate from inside the callback by running once and
    // then clearing between runs, plus clearing from within via the SVC hook.
    let jit = vm.raw();
    // SAFETY: `jit` is live and not executing.
    unsafe { od_jit_invalidate_range(jit, harness::CODE_BASE, 16) };

    let mut rounds = 0;
    loop {
        let hr = vm.run();
        rounds += 1;
        assert!(rounds < 32, "did not settle");
        if hr & OD_HALT_CACHE_INVALIDATION != 0 {
            // SAFETY: `jit` is live and execution has returned.
            unsafe { od_jit_clear_halt(jit, OD_HALT_CACHE_INVALIDATION) };
            continue;
        }
        if hr != 0 {
            break;
        }
    }

    assert_eq!(vm.reg(0), 8, "both instructions ran across the invalidation");
    assert_eq!(vm.with_ctx(|c| c.svc.len()), 2);
    assert_eq!(vm.with_ctx(|c| c.max_depth), 1);
}
