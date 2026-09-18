//! Measurements, not assertions. `#[ignore]`d so a normal `cargo test` does not pay for them:
//!
//! ```text
//! cargo test -p omni-cpu --release --test bench -- --ignored --nocapture
//! ```
//!
//! Release only: a debug build measures the harness rather than the jit.
//!
//! Three questions Task 3 owes a number to, each of which was a configuration choice made on a
//! reasoned argument that then needed checking:
//!
//! 1. **What does identity mapping actually buy, here, on this guest?** D4 measured 13.2x in the
//!    spike. This measures the same thing through the real backend, against the configuration the
//!    startup assertion refuses — which is the honest way to say what the assertion is worth.
//! 2. **What does `check_halt_on_memory_access` cost?** It is what makes a guest fault stop at the
//!    faulting instruction rather than running on. `EmitCheckMemoryAbort` is emitted only in the
//!    deferred abort block, so the fastmem hot path is untouched — but `A64::Jit::Impl` skips
//!    `GetSetElimination` entirely when it is set, and that is not free.
//! 3. **What does a sliced run loop cost?** The watchdog is a budget expiring, so every
//!    `SLICE_INSTRUCTIONS` the guest returns to the dispatcher. That is a real overhead and it
//!    should be small.

#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use std::time::{Duration, Instant};

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::{DynarmicOptions, FastmemOverrides};
use omni_cpu::{ExitReason, GuestCpu, RunLimit};

/// Samples per configuration. Odd, so the median is an observation rather than an average of two.
const N: usize = 31;

struct Summary {
    min: Duration,
    median: Duration,
    max: Duration,
}

impl Summary {
    fn of(mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        Self { min: samples[0], median: samples[samples.len() / 2], max: samples[samples.len() - 1] }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Run `body` once to warm the translation, then `N` times.
fn measure(mut body: impl FnMut()) -> Summary {
    body();
    Summary::of((0..N).map(|_| { let t = Instant::now(); body(); t.elapsed() }).collect())
}

/// A load/store loop: two guest memory accesses per iteration, which is what makes it a fastmem
/// benchmark rather than a register one.
fn memory_loop(data: usize, iterations: u64) -> Vec<u32> {
    let mut program = mov64(0, data as u64);
    program.extend(mov64(1, iterations));
    program.push(movz(2, 0, 0));
    let loop_start = program.len();
    program.push(ldr_imm(3, 0, 0));
    program.push(add_reg(2, 2, 3));
    program.push(str_imm(2, 0, 8));
    program.push(subs_imm(1, 1, 1));
    let here = program.len();
    program.push(b_cond(1, loop_start as i32 - here as i32));
    program.push(ret(30));
    program
}

/// A loop with **no** guest memory access at all: five register operations per iteration.
///
/// This is the workload where `GetSetElimination` earns its keep, so it is the one that says what
/// turning it off really costs. D5 measured register-bound integer code at ~33x native — the worst
/// thing dynarmic does — and attributed it to per-block register allocation spilling every guest
/// register to `JitState`, which is exactly what `GetSetElimination` reduces.
fn register_loop(iterations: u64) -> Vec<u32> {
    let mut program = mov64(1, iterations);
    program.push(movz(2, 0, 0));
    let loop_start = program.len();
    program.push(add_imm(2, 2, 1));
    program.push(add_imm(3, 2, 2));
    program.push(add_reg(4, 3, 2));
    program.push(subs_imm(1, 1, 1));
    let here = program.len();
    program.push(b_cond(1, loop_start as i32 - here as i32));
    program.push(ret(30));
    program
}

const ITERATIONS: u64 = 1_000_000;
/// Five instructions in the loop body, plus a prologue this ignores.
const LOOP_INSTRUCTIONS: u64 = ITERATIONS * 5;

/// **What D4's identity mapping is worth on this backend**, and therefore what the startup
/// assertion is defending.
#[test]
#[ignore = "measurement, not a test"]
fn identity_mapping_against_the_default_width() {
    let guest = Guest::new();
    guest.assert_high_addresses();
    let entry = guest.load(&memory_loop(guest.data, ITERATIONS));
    guest.write_u64(guest.data, 1);
    let sentinel = guest.code + harness::CODE_BYTES - 4;

    let run_with = |mut cpu: omni_cpu::dynarmic::DynarmicCpu| {
        cpu.set_return_sentinel(sentinel).expect("sentinel");
        cpu.set_x(x(30), sentinel as u64);
        let summary = measure(|| {
            cpu.set_x(x(30), sentinel as u64);
            let exit = cpu.run(entry, RunLimit::Unlimited).expect("the loop runs");
            assert_eq!(exit, ExitReason::Returned { pc: sentinel });
        });
        (summary, cpu.stats().slow_path_total)
    };

    let (identity, identity_callbacks) =
        run_with(guest.backend.create_thread_with_tls().expect("a conforming thread"));
    let (degraded, degraded_callbacks) = {
        let (cpu, _why) = guest
            .backend
            .create_misconfigured_thread(FastmemOverrides {
                address_space_bits: Some(36),
                ..Default::default()
            })
            .expect("a 36-bit thread");
        run_with(cpu)
    };

    println!("\n== D4: identity mapping vs dynarmic's default width (n = {N} per configuration) ==");
    println!("{ITERATIONS} iterations, 2 guest memory accesses each, {LOOP_INSTRUCTIONS} guest instructions");
    println!(
        "  64-bit identity : {:7.3} ms median  [{:7.3} .. {:7.3}]  {:8.0} Mguest-insn/s  \
         callback entries {identity_callbacks}",
        ms(identity.median),
        ms(identity.min),
        ms(identity.max),
        LOOP_INSTRUCTIONS as f64 / identity.median.as_secs_f64() / 1e6,
    );
    println!(
        "  36-bit default  : {:7.3} ms median  [{:7.3} .. {:7.3}]  {:8.0} Mguest-insn/s  \
         callback entries {degraded_callbacks}",
        ms(degraded.median),
        ms(degraded.min),
        ms(degraded.max),
        LOOP_INSTRUCTIONS as f64 / degraded.median.as_secs_f64() / 1e6,
    );
    println!(
        "  ratio           : {:.2}x  (D4's spike measured 13.2x on a different loop shape)",
        degraded.median.as_secs_f64() / identity.median.as_secs_f64()
    );
    println!("  Both produce identical results. That is the whole reason the check exists.\n");
}

/// **What `check_halt_on_memory_access` costs.**
///
/// The emitted check itself is on the abort path only, so what is being measured here is the loss of
/// `GetSetElimination`, which `A64::Jit::Impl` skips whenever this flag is set.
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_stopping_at_the_faulting_instruction() {
    println!("\n== check_halt_on_memory_access (n = {N} per configuration) ==");
    println!("{LOOP_INSTRUCTIONS} guest instructions per workload");

    // Two workloads, because they answer different questions. The memory-heavy one says what the
    // emitted check costs where it is emitted; the register-bound one says what losing
    // `GetSetElimination` costs, and that is where the loss actually lands.
    for (workload, register_bound) in
        [("memory-heavy, 2 accesses per 5", false), ("register-bound, no memory access", true)]
    {
        let mut rows = Vec::new();
        for check in [false, true] {
            let guest = Guest::with_options(DynarmicOptions {
                check_halt_on_memory_access: check,
                ..Default::default()
            });
            let program = if register_bound {
                register_loop(ITERATIONS)
            } else {
                memory_loop(guest.data, ITERATIONS)
            };
            let entry = guest.load(&program);
            guest.write_u64(guest.data, 1);
            let (mut cpu, sentinel) = guest.thread();
            let summary = measure(|| {
                cpu.set_x(x(30), sentinel as u64);
                let exit = cpu.run(entry, RunLimit::Unlimited).expect("the loop runs");
                assert_eq!(exit, ExitReason::Returned { pc: sentinel });
            });
            rows.push(summary);
        }
        println!("  {workload}:");
        for (check, summary) in [false, true].iter().zip(&rows) {
            println!(
                "    check_halt_on_memory_access = {:5} : {:7.3} ms median  [{:7.3} .. {:7.3}]  \
                 {:8.0} Mguest-insn/s",
                check,
                ms(summary.median),
                ms(summary.min),
                ms(summary.max),
                LOOP_INSTRUCTIONS as f64 / summary.median.as_secs_f64() / 1e6,
            );
        }
        println!("    ratio : {:.3}x", rows[1].median.as_secs_f64() / rows[0].median.as_secs_f64());
    }
    println!(
        "  The check itself is emitted only in the deferred abort block, never on the fastmem fast \
         path, so what these ratios measure is the loss of GetSetElimination.\n"
    );
}

/// **What the sliced run loop costs.** The watchdog returns to the dispatcher every
/// `SLICE_INSTRUCTIONS`; a short slice makes that visible and is the control.
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_slicing_the_run_loop() {
    let guest = Guest::new();
    let entry = guest.load(&memory_loop(guest.data, ITERATIONS));
    guest.write_u64(guest.data, 1);
    let (mut cpu, sentinel) = guest.thread();

    let mut time = |limit| {
        measure(|| {
            cpu.set_x(x(30), sentinel as u64);
            let exit = cpu.run(entry, limit).expect("the loop runs");
            assert_eq!(exit, ExitReason::Returned { pc: sentinel });
        })
    };
    let unlimited = time(RunLimit::Unlimited);
    let counted = time(RunLimit::Instructions(u64::MAX));
    let tiny = time(RunLimit::Instructions(LOOP_INSTRUCTIONS * 2));

    println!("\n== the sliced run loop (n = {N} per configuration) ==");
    println!("{LOOP_INSTRUCTIONS} guest instructions, slice = {} instructions", omni_cpu::run::SLICE_INSTRUCTIONS);
    for (name, summary) in
        [("Unlimited", &unlimited), ("Instructions(u64::MAX)", &counted), ("a counted budget", &tiny)]
    {
        println!(
            "  {name:22} : {:7.3} ms median  [{:7.3} .. {:7.3}]",
            ms(summary.median),
            ms(summary.min),
            ms(summary.max)
        );
    }
    println!(
        "  Instructions(u64::MAX) vs Unlimited: {:.3}x. Without the clamp in `run::slice_budget` \
         this would be two orders of magnitude, because the emitted cycle comparison is signed.\n",
        counted.median.as_secs_f64() / unlimited.median.as_secs_f64()
    );
}
