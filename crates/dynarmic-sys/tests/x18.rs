//! **The guest's X18 survives the host's X18 being the platform's.**
//!
//! Apple's arm64 ABI reserves `x18` for the platform, and the kernel does not preserve it for user
//! code: a value left there can be gone after any context switch. Android's bionic reserves nothing
//! of the kind (`x18` is a platform register there only under ShadowCallStack), so guest code may
//! keep live values in `X18` across anything. The two are compatible only if the translator never
//! keeps the guest's `X18` -- or anything else -- in the *host's* `x18`.
//!
//! What the arm64 backend does, read from the vendored source (and checked by the first test):
//! guest registers live in `A64JitState::reg`, and are brought into host registers only by the
//! register allocator, which allocates from `GPR_ORDER` (`backend/arm64/abi.h`) --
//! `{19..23, 9..15, 0..8}` -- plus the named fixed registers `Xstate` (28), `Xhalt` (27), `Xticks`
//! (26), `Xfastmem` (25), `Xpagetable` (24) and the scratch registers 16, 17 and 30. Host `x18` is in
//! none of them, and no emitter names it. The second and third tests then *measure* it: a guest
//! value in `X18` across thousands of forced host context switches, and across a long compute loop.

mod harness;

use harness::a64;
use harness::{Vm, VmOptions, HALT_DONE};

#[test]
fn the_arm64_backend_never_allocates_or_names_host_x18() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/dynarmic/src/dynarmic/backend/arm64");
    let abi = std::fs::read_to_string(dir.join("abi.h")).expect("abi.h");
    let order = abi
        .lines()
        .find(|l| l.contains("GPR_ORDER{"))
        .expect("GPR_ORDER is declared in abi.h");
    let inside = &order[order.find('{').unwrap() + 1..order.find('}').unwrap()];
    let regs: Vec<u32> = inside.split(',').map(|x| x.trim().parse().expect("a register number")).collect();
    assert!(!regs.contains(&18), "the allocator may hand out x18: {regs:?}");
    for fixed in ["Xstate{28}", "Xhalt{27}", "Xticks{26}", "Xfastmem{25}", "Xpagetable{24}", "Xscratch0{16}, Xscratch1{17}, Xscratch2{30}"] {
        assert!(abi.contains(fixed), "abi.h no longer fixes {fixed}");
    }
    let mut named = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("arm64 backend") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "cpp" || e == "h") {
            let text = std::fs::read_to_string(&path).expect("read");
            for needle in ["X18", "W18", "XReg{18}", "WReg{18}"] {
                if text.contains(needle) {
                    named.push(format!("{} names {needle}", path.display()));
                }
            }
        }
    }
    assert!(named.is_empty(), "an arm64 emitter names host x18: {named:?}");
}

/// `X18` and `X19` start equal and are incremented together; any divergence is the host having
/// had its way with one of them.
const PATTERN: u64 = 0xA5A5_5A5A_1818_0000;

#[test]
fn guest_x18_survives_thousands_of_host_context_switches() {
    // loop: SVC #1 (the host thread sleeps 100 us) ; ADD X18, X18, #1 ; ADD X19, X19, #1 ;
    //       SUBS X5, X5, #1 ; B.NE loop ; SVC #0
    const ROUNDS: u64 = 2_000;
    let code = vec![
        a64::svc(1),
        a64::add_imm(18, 18, 1),
        a64::add_imm(19, 19, 1),
        a64::subs_imm(5, 5, 1),
        a64::b_cond(a64::cond::NE, -4),
        a64::svc(0),
    ];
    let vm = Vm::new(code, VmOptions { cycle_counting: false, ..VmOptions::default() });
    vm.with_ctx(|c| c.sleep_on_svc1_us = 100);
    vm.set_reg(18, PATTERN);
    vm.set_reg(19, PATTERN);
    vm.set_reg(5, ROUNDS);
    vm.start(u64::MAX >> 1);
    let started = std::time::Instant::now();
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    let elapsed = started.elapsed();
    assert_eq!(vm.with_ctx(|c| c.svc.len()) as u64, ROUNDS + 1, "every SVC #1 was taken");
    // 2,000 sleeps of 100 us cannot finish in under 0.2 s: the thread really was descheduled.
    assert!(elapsed >= std::time::Duration::from_millis(200), "slept only {elapsed:?}");
    assert_eq!(vm.reg(19), PATTERN + ROUNDS);
    assert_eq!(vm.reg(18), PATTERN + ROUNDS, "guest X18 diverged from its twin X19");
}

#[test]
fn guest_x18_survives_a_long_compute_loop() {
    // loop: ADD X18, X18, #3 ; ADD X19, X19, #3 ; SUBS X5, X5, #1 ; B.NE loop ; SVC #0
    // 200 million iterations, long enough to be preempted by the scheduler many times over.
    const ROUNDS: u64 = 200_000_000;
    let code = vec![
        a64::add_imm(18, 18, 3),
        a64::add_imm(19, 19, 3),
        a64::subs_imm(5, 5, 1),
        a64::b_cond(a64::cond::NE, -3),
        a64::svc(0),
    ];
    // A budget of twice the loop's 4 * ROUNDS instructions, so that a clobbered *counter* ends in
    // a failed assertion rather than a loop that never terminates. MEASURED: with host x18 put first
    // in GPR_ORDER (a mutation), this loop did not finish in 600 s without the budget -- the
    // allocator had given it x18, and the kernel had zeroed it.
    let vm = Vm::new(code, VmOptions { cycle_counting: true, ..VmOptions::default() });
    vm.set_reg(18, PATTERN);
    vm.set_reg(19, PATTERN);
    vm.set_reg(5, ROUNDS);
    vm.start(8 * ROUNDS);
    let hr = vm.run_to_completion(1_000_000);
    assert_eq!(hr & HALT_DONE, HALT_DONE, "the loop did not end inside its budget (halt {hr:#x}, X5 = {:#x})", vm.reg(5));
    assert_eq!(vm.reg(19), PATTERN + 3 * ROUNDS);
    assert_eq!(vm.reg(18), PATTERN + 3 * ROUNDS, "guest X18 diverged from its twin X19");
}
