//! **I3: `CNTPCT_EL0` as the guest sees it.**
//!
//! The counter used to be the backend's per-slice instruction count, which `run` resets at the top
//! of every slice and of every run. So a guest reading it as a clock saw it sawtooth, and the
//! comment above the callback said "monotonic". Its units were wrong too: guest instructions,
//! against an advertised 600 MHz.
//!
//! Neither half had a test. Both halves are here, and they are driven by **guest `MRS`
//! instructions**, because the property belongs to the guest's view and not to a Rust function: a
//! test that called `clock::cntpct` twice would pass over a backend that never wired it up.
//!
//! What a guest does with the defect is why it is worth this much test: `now() - then()` is
//! negative, a spin-until-deadline loop never terminates, and the run is eventually stopped by the
//! step budget and reported as `StepLimitReached`. Every symptom points somewhere other than the
//! clock.

#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use std::time::{Duration, Instant};

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::run::SLICE_INSTRUCTIONS;
use omni_cpu::{ExitReason, GuestCpu, RunLimit, CNTFRQ_HZ};

/// Busy-wait for a known interval. `sleep` is ~15 ms granular on Windows and the point of these
/// tests is to bound the counter against an elapsed time that is actually known.
fn spin_for(wait: Duration) -> Duration {
    let start = Instant::now();
    while start.elapsed() < wait {
        std::hint::spin_loop();
    }
    start.elapsed()
}

/// The encodings, checked against the ARM ARM by eye and against the encoder by assertion.
#[test]
fn the_counter_registers_encode_as_the_manual_says() {
    // MRS Xt, CNTFRQ_EL0 is S3_3_C14_C0_0; MRS Xt, CNTPCT_EL0 is S3_3_C14_C0_1. The two differ in
    // `op2` alone, which is bits 7:5, so the second is the first plus 0x20.
    assert_eq!(mrs_cntfrq_el0(0), 0xD53B_E000);
    assert_eq!(mrs_cntpct_el0(0), 0xD53B_E020);
    assert_eq!(mrs_cntpct_el0(0), mrs_cntfrq_el0(0) | 0x20);
    // And `Rt` is the low five bits.
    assert_eq!(mrs_cntpct_el0(9), 0xD53B_E029);
}

/// The guest must read back the frequency this backend programmed, exactly (Global Constraint 3).
///
/// The two are the same constant on our side — `clock::CNTFRQ_HZ` is written into the jit's
/// `cntfrq_el0` *and* used to scale `CNTPCT_EL0` — and this is what checks that the constant
/// actually arrives, rather than dynarmic's default happening to match it.
#[test]
fn the_guest_reads_the_frequency_the_backend_advertises() {
    let guest = Guest::new();
    let entry = guest.load(&[mrs_cntfrq_el0(0), ret(30)]);
    let (mut cpu, sentinel) = guest.thread();
    cpu.set_x(x(30), sentinel as u64);

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a run that returns");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit}");
    assert_eq!(
        cpu.x(x(0)),
        u64::from(CNTFRQ_HZ),
        "the guest divides CNTPCT_EL0 by this to get seconds, so it has to be the frequency the \
         counter is actually scaled to"
    );
}

/// **The defect, in the shape a guest meets it: two reads across two runs.**
///
/// Every `run` reset the old counter to zero, so the second read was smaller than the first however
/// much time had passed. This also pins the units, which a monotonicity check cannot: the counter
/// must advance by at least the ticks that the *measured* elapsed host time is worth, and by no more
/// than the ticks the whole test took. With the per-slice instruction counter the delta was a
/// handful of ticks against a lower bound of twelve million.
#[test]
fn the_counter_does_not_reset_between_runs_and_advances_in_real_time() {
    const WAIT: Duration = Duration::from_millis(20);

    let guest = Guest::new();
    let entry = guest.load(&[mrs_cntpct_el0(0), ret(30)]);
    let (mut cpu, sentinel) = guest.thread();

    // Warm the translation before the window opens. The ceiling below is the real time that passed
    // over the *whole* measured window, so a cold first run inflates it and makes the check weaker
    // than it looks -- weak enough, measured, to let "the counter returns nanoseconds" through.
    cpu.set_x(x(30), sentinel as u64);
    cpu.run(entry, RunLimit::Unlimited).expect("a warm-up run");

    let wall = Instant::now();
    cpu.set_x(x(30), sentinel as u64);
    cpu.run(entry, RunLimit::Unlimited).expect("the first run");
    let first = cpu.x(x(0));

    let waited = spin_for(WAIT);

    cpu.set_x(x(30), sentinel as u64);
    cpu.run(entry, RunLimit::Unlimited).expect("the second run");
    let second = cpu.x(x(0));
    let wall_elapsed = wall.elapsed();

    assert!(
        second > first,
        "the guest's clock went backwards across two runs: {first} then {second}. A guest \
         subtracting these gets a negative interval, and a spin-until-deadline loop never ends"
    );

    let delta = second - first;
    println!(
        "CNTPCT_EL0 across two runs: {delta} ticks over {wall_elapsed:?} of real time \
         (n = 1 interval, spun for {waited:?}); CNTFRQ_EL0 = {CNTFRQ_HZ} Hz"
    );
    let ticks_per_nano = |d: Duration| {
        (d.as_nanos() * u128::from(CNTFRQ_HZ) / 1_000_000_000) as u64
    };
    let floor = ticks_per_nano(waited);
    let ceiling = ticks_per_nano(wall_elapsed);
    assert!(
        delta >= floor,
        "n = 1 interval of {waited:?}: the counter advanced {delta} ticks, but {waited:?} at \
         {CNTFRQ_HZ} Hz is {floor}. A counter that advances more slowly than the frequency it \
         advertises makes every guest timeout too long"
    );
    assert!(
        delta <= ceiling,
        "the counter advanced {delta} ticks while only {wall_elapsed:?} ({ceiling} ticks) of real \
         time passed over the whole test, so it is running fast"
    );
}

/// The same read, twice inside **one** run that spans more than one slice.
///
/// This is the review's literal finding — `run` resets the counter at the top of every slice, not
/// only of every run — and it needs a program long enough to cross a slice boundary with a read on
/// each side. The first read is late in slice one and the second early in slice two, which is the
/// only arrangement that discriminates: two reads both early in their own slices would be increasing
/// even with the defect present.
#[test]
fn the_counter_does_not_reset_between_slices_of_one_run() {
    // A two-instruction loop, so the instruction count is twice the iteration count.
    fn countdown(reg: u32, iterations: u64) -> Vec<u32> {
        let mut out = mov64(reg, iterations);
        out.push(subs_imm(reg, reg, 1));
        out.push(b_cond(1, -1)); // B.NE back to the SUBS
        out
    }

    // ~900,000 instructions, then a read; then ~300,000 more, then a second read. The slice is
    // 1,000,000, so the second read is in the slice after the first.
    let mut program = countdown(3, 450_000);
    program.push(mrs_cntpct_el0(1));
    program.extend(countdown(4, 150_000));
    program.push(mrs_cntpct_el0(2));
    program.push(ret(30));

    let guest = Guest::new();
    let entry = guest.load(&program);
    let (mut cpu, sentinel) = guest.thread();
    cpu.set_x(x(30), sentinel as u64);

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a run that returns");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit}");

    let executed = cpu.last_run_instructions();
    assert!(
        executed > SLICE_INSTRUCTIONS,
        "this run executed {executed} instructions against a slice of {SLICE_INSTRUCTIONS}, so it \
         never crossed a slice boundary and the test proves nothing"
    );

    let first = cpu.x(x(1));
    let second = cpu.x(x(2));
    assert!(
        second >= first,
        "the guest's clock went backwards inside one run, across a slice boundary: {first} then \
         {second}, over {executed} guest instructions"
    );
}

/// **One counter, not one per thread.** `CNTPCT_EL0` is the *system* counter: every core reads the
/// same one, so two guest threads that timestamp an event must agree about when it happened.
///
/// The discriminating arrangement is a measured gap: this thread reads first, real time passes, and
/// a second guest thread reads. A per-context or per-thread epoch would restart the second thread's
/// counter near zero, and it would read *less* than a thread that started earlier — which is the
/// tidier-looking implementation and the one this rules out. D5 measured Roblox to be heavily
/// multithreaded, so "which thread read it" is not a hypothetical distinction.
#[test]
fn two_guest_threads_read_one_shared_counter() {
    const GAP: Duration = Duration::from_millis(30);

    let guest = Guest::new();
    let entry = guest.load(&[mrs_cntpct_el0(0), ret(30)]);

    let (mut first_cpu, sentinel) = guest.thread();
    first_cpu.set_x(x(30), sentinel as u64);
    first_cpu.run(entry, RunLimit::Unlimited).expect("the first thread's run");
    let first = first_cpu.x(x(0));

    let waited = spin_for(GAP);

    let (mut second_cpu, second_sentinel) = guest.thread();
    let second = std::thread::spawn(move || {
        second_cpu.set_x(x(30), second_sentinel as u64);
        second_cpu.run(entry, RunLimit::Unlimited).expect("the second thread's run");
        second_cpu.x(x(0))
    })
    .join()
    .expect("the second guest thread");

    assert!(
        second > first,
        "a guest thread created {waited:?} later read {second}, which is not after the {first} a \
         thread on another host thread read. CNTPCT_EL0 is the system counter, so this is two \
         cores disagreeing about what time it is"
    );
    let floor = (waited.as_nanos() * u128::from(CNTFRQ_HZ) / 1_000_000_000) as u64;
    assert!(
        second - first >= floor,
        "n = 1 gap of {waited:?} ({floor} ticks): the two threads' readings differ by only {}, \
         which is what a per-thread epoch looks like",
        second - first
    );
}
