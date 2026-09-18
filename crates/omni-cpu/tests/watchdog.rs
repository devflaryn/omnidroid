//! The run loop: what actually stops untrusted guest code, and what does not.
//!
//! # What was chosen, and why
//!
//! Task 2 measured that `EmitTerminalImpl(IR::Term::LinkBlock)` emits **one** check — the cycle
//! counter when `enable_cycle_counting` is on, the halt flag when it is off, never both — so a
//! block-linked guest loop ignores whichever escape it was not compiled to check. Three
//! configurations were available, and this is the one taken:
//!
//! | Configuration | Direct-branch loop | Indirect-branch loop | Cost |
//! |---|---|---|---|
//! | `ALL_SAFE`, cycle counting | budget stops it | **nothing stops it** | baseline |
//! | `INTERRUPTIBLE`, cycle counting | budget stops it | budget stops it | ~3.9 ns per indirect transfer (n = 31): 0x on code with no indirect branches, up to ~5x on indirect-saturated code |
//! | `BlockLinking` off | both escapes work on both | both escapes work on both | **~6.6x** on a 4-instruction-per-block workload (n = 31) |
//!
//! **`INTERRUPTIBLE` with block linking left on.** The third row buys a second escape that is not
//! needed: once the budget stops every guest shape, the halt flag is redundant *inside* a slice,
//! because the run loop checks it in Rust between slices and a slice is about 200 µs of guest time.
//! Paying 6.6x for a mechanism whose only advantage is latency the caller cannot perceive is the
//! wrong trade — and 6.6x is an upper bound driven by block length, which makes it worst on exactly
//! the register-bound code D5 already measured at 33x native.
//!
//! What that costs instead is the second row's 0x-to-5x band, which is why `libroblox.so`'s real
//! branch mix is measured and reported rather than guessed at.
//!
//! # What is not tested here
//!
//! The negative direction — a guest loop that **cannot** be stopped under `ALL_SAFE` — is Task 2's
//! 18-cell matrix, each cell in its own child process. It is not reproduced here on purpose: the
//! failure mode is a host thread that spins forever, and a test that wedges the runner is not a
//! test. [`the_configuration_that_makes_the_budget_work_is_in_effect`] pins the configuration
//! instead, which is the part this crate controls.

#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use std::time::{Duration, Instant};

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::DynarmicOptions;
use omni_cpu::{ExitReason, GuestCpu, RunLimit};

/// `B .` — a direct branch to itself, the cheapest guest loop that never ends.
///
/// The offset is **0**, not -1: an A64 branch displacement is relative to the branch's own address,
/// so `b(-1)` is `B #-4` and would leave the code region rather than spin.
fn direct_spin() -> Vec<u32> {
    vec![b(0)]
}

/// `BR X0` with `X0` pointing at the `BR` itself: an indirect branch to itself, which is the shape
/// Task 2 found unstoppable by **either** escape under the default optimization flags.
fn indirect_spin(entry: usize) -> Vec<u32> {
    let mut program = mov64(0, (entry + 4 * 4) as u64);
    while program.len() < 4 {
        program.insert(0, NOP);
    }
    program.push(br(0));
    program
}

#[test]
fn a_direct_branch_loop_is_stopped_by_a_step_budget() {
    let guest = Guest::new();
    let entry = guest.load(&direct_spin());
    let (mut cpu, _) = guest.thread();

    let started = Instant::now();
    let exit = cpu.run(entry, RunLimit::Instructions(50_000)).expect("the spin runs");
    let elapsed = started.elapsed();

    match exit {
        ExitReason::StepLimitReached { pc, executed } => {
            assert_eq!(pc, entry, "a self-branch never leaves its own address");
            assert!(
                executed >= 50_000,
                "the budget must have been spent, not merely offered: {executed} executed"
            );
        }
        other => panic!("a `B .` loop must stop on its step budget, got {other}"),
    }
    assert!(elapsed < Duration::from_secs(5), "and it must stop promptly: took {elapsed:?}");
}

/// The shape Task 2 found unstoppable under the default flags. Under this backend's configuration
/// the budget stops it, which is the whole reason `interruptible` defaults to `true`.
#[test]
fn an_indirect_branch_loop_is_stopped_by_a_step_budget() {
    let guest = Guest::new();
    let entry = guest.code;
    let program = indirect_spin(entry);
    assert_eq!(guest.load(&program), entry);
    let (mut cpu, _) = guest.thread();

    let started = Instant::now();
    let exit = cpu.run(entry, RunLimit::Instructions(50_000)).expect("the spin runs");
    let elapsed = started.elapsed();

    assert!(
        matches!(exit, ExitReason::StepLimitReached { executed, .. } if executed >= 50_000),
        "a `BR X0`-to-self loop must stop on its step budget under optimization::INTERRUPTIBLE, \
         got {exit}"
    );
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    assert_eq!(cpu.x(x(0)), (entry + 16) as u64, "and X0 still holds the branch target");
}

/// An unlimited run is sliced, so a halt from another thread lands between slices.
///
/// This is what `Capabilities::asynchronous_halt` claims, tested rather than asserted.
#[test]
fn an_unlimited_run_of_a_spinning_guest_is_stopped_by_a_halt_between_slices() {
    let guest = Guest::new();
    let entry = guest.load(&direct_spin());
    let (mut cpu, _) = guest.thread();
    assert!(cpu.capabilities().asynchronous_halt, "this configuration claims the halt works");

    let handle = cpu.halt_handle();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        handle.request();
    });

    let started = Instant::now();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the spin runs");
    let elapsed = started.elapsed();
    stopper.join().expect("the stopping thread");

    assert_eq!(
        exit,
        ExitReason::Halted { pc: entry },
        "an unlimited run of a guest that never stops must be stopped by the halt flag, got {exit}"
    );
    assert!(exit.is_resumable());
    assert!(
        elapsed < Duration::from_secs(10),
        "the halt is observed between slices, so it must land within about a slice: {elapsed:?}"
    );
    println!(
        "halt latency on a `B .` guest (1 run): {elapsed:?}, slice = {} guest instructions",
        omni_cpu::run::SLICE_INSTRUCTIONS
    );
}

/// The configuration the budget depends on, read back from dynarmic rather than assumed.
#[test]
fn the_configuration_that_makes_the_budget_work_is_in_effect() {
    let guest = Guest::new();
    let (cpu, _) = guest.thread();
    let config = cpu.effective_config();

    assert_eq!(config.enable_cycle_counting, 1, "no counting means no budget to expire");

    // `INTERRUPTIBLE` is `ALL_SAFE` without the two flags whose terminal handlers check neither the
    // cycle counter nor the halt flag.
    const RETURN_STACK_BUFFER: u32 = 0x02;
    const FAST_DISPATCH: u32 = 0x04;
    const BLOCK_LINKING: u32 = 0x01;
    assert_eq!(
        config.optimizations & (RETURN_STACK_BUFFER | FAST_DISPATCH),
        0,
        "the return-stack-buffer and fast-dispatch terminal handlers jump straight from block to \
         block checking nothing, which is what makes an indirect-branch loop unstoppable"
    );
    assert_ne!(
        config.optimizations & BLOCK_LINKING,
        0,
        "block linking stays ON: turning it off buys a second escape the run loop does not need, \
         for a measured ~6.6x on a 4-instruction-per-block workload (n = 31)"
    );

    // And the opposite configuration reports itself honestly rather than claiming a halt it has not
    // got.
    let permissive = Guest::with_options(DynarmicOptions { interruptible: false, ..Default::default() });
    let (permissive_cpu, _) = permissive.thread();
    assert!(
        !permissive_cpu.capabilities().asynchronous_halt,
        "with the two flags left in, an indirect-branch guest loop cannot be stopped, and \
         Capabilities must say so rather than promising an escape that does not exist"
    );
    assert_ne!(permissive_cpu.effective_config().optimizations & FAST_DISPATCH, 0);
}

/// A zero budget is a budget, not an absence of one.
#[test]
fn a_zero_step_budget_executes_nothing() {
    let guest = Guest::new();
    let entry = guest.load(&[movz(0, 0x1234, 0), ret(30)]);
    let (mut cpu, _) = guest.thread();
    cpu.set_x(x(0), 0xAAAA);

    let exit = cpu.run(entry, RunLimit::Instructions(0)).expect("a zero budget is defined");
    assert_eq!(exit, ExitReason::StepLimitReached { pc: entry, executed: 0 }, "{exit}");
    assert_eq!(cpu.x(x(0)), 0xAAAA, "nothing may have executed");
}

/// The `u64::MAX` footgun at the level of a running guest: the value a caller naturally writes for
/// "no limit" must behave like no limit, not like the worst budget available.
///
/// The emitted check is `cmp qword[…cycles_remaining], 0` followed by `jg` — signed — so passing
/// `u64::MAX` straight through would read as `-1` and return to the dispatcher after **every
/// block**. `Budget::slice` clamps, and this is the end-to-end proof that the clamp is on the path
/// a caller actually takes.
#[test]
fn an_unlimited_counted_budget_is_not_slower_than_an_unlimited_run() {
    let guest = Guest::new();
    // A loop with many short blocks, which is where a per-block return would show up worst.
    let mut program = mov64(1, 200_000);
    let loop_start = program.len();
    program.push(subs_imm(1, 1, 1));
    let here = program.len();
    program.push(b_cond(1, loop_start as i32 - here as i32));
    program.push(ret(30));
    let entry = guest.load(&program);

    let (mut cpu, sentinel) = guest.thread();
    // Warm the translation so neither timing includes cold translation.
    cpu.run(entry, RunLimit::Unlimited).expect("warm-up");

    let mut time = |limit| {
        let started = Instant::now();
        let exit = cpu.run(entry, limit).expect("the loop runs");
        assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
        started.elapsed()
    };
    let unlimited = time(RunLimit::Unlimited);
    let max = time(RunLimit::Instructions(u64::MAX));

    println!(
        "200,000-iteration loop, warm, n = 1 run each: Unlimited {unlimited:?}, \
         Instructions(u64::MAX) {max:?}"
    );
    assert!(
        max < unlimited * 20 + Duration::from_millis(50),
        "Instructions(u64::MAX) took {max:?} against {unlimited:?} for Unlimited. A signed \
         comparison against a budget with bit 63 set returns after every block, so this is the \
         shape that footgun takes"
    );
}

/// A counted budget spread over several slices reports the real total, and resumes where it left
/// off rather than restarting.
#[test]
fn a_long_counted_run_spans_slices_and_reports_the_real_total() {
    let guest = Guest::new();
    let entry = guest.load(&direct_spin());
    let (mut cpu, _) = guest.thread();

    let budget = omni_cpu::run::SLICE_INSTRUCTIONS * 3 + 7;
    let exit = cpu.run(entry, RunLimit::Instructions(budget)).expect("the spin runs");
    match exit {
        ExitReason::StepLimitReached { executed, .. } => {
            assert!(executed >= budget, "{executed} executed against a budget of {budget}");
            // The overshoot is bounded by a block, not by a slice: a slice boundary is a return to
            // the dispatcher, not a place the count can run away.
            assert!(
                executed < budget + omni_cpu::run::SLICE_INSTRUCTIONS,
                "overshoot of {} is larger than one slice, which means a slice boundary lost count",
                executed - budget
            );
        }
        other => panic!("expected the budget to expire, got {other}"),
    }
}
