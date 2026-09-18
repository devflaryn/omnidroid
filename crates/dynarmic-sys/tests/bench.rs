//! Measurements, not assertions. `#[ignore]`d so a normal `cargo test` does not
//! pay for them:
//!
//! ```text
//! cargo test -p dynarmic-sys --release --test bench -- --ignored --nocapture
//! ```
//!
//! Release only. A debug build measures the harness, not the jit.
//!
//! Two questions these exist to answer, both of which had been argued rather
//! than measured:
//!
//! 1. What does `optimization::INTERRUPTIBLE` cost? It is the only
//!    configuration in which an indirect-branch runaway can be stopped at all,
//!    and it works by removing dispatch acceleration from every `RET`, `BLR`
//!    and `BR`. "A real cost" is not a number.
//! 2. What does `DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT` cost? With it off,
//!    dynarmic's code cache is `PAGE_EXECUTE_READWRITE`, which contradicts D12.
//!    With it on, every translated block is bracketed by a `VirtualProtect`
//!    pair over the whole committed region, so the cost lands on cold
//!    translation — D5 risk 1, already the weak spot.

mod harness;

use dynarmic_sys::optimization;
use harness::a64::{self, cond};
use harness::{Vm, VmOptions, CODE_BASE, HALT_DONE};
use std::time::{Duration, Instant};

/// Samples per configuration. Odd, so the median is an observation.
const N: usize = 31;

struct Summary {
    min: Duration,
    median: Duration,
    max: Duration,
}

impl Summary {
    fn of(mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        Self {
            min: samples[0],
            median: samples[samples.len() / 2],
            max: samples[samples.len() - 1],
        }
    }

    fn rate(&self, instructions: u64) -> f64 {
        instructions as f64 / self.median.as_secs_f64() / 1e6
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Runs `body` `N` times after one warm-up, returning the timings.
fn measure(mut body: impl FnMut()) -> Summary {
    body();
    let mut samples = Vec::with_capacity(N);
    for _ in 0..N {
        let started = Instant::now();
        body();
        samples.push(started.elapsed());
    }
    Summary::of(samples)
}

/// A loop saturated with indirect transfers: `BLR` out and `RET` back, twice
/// per iteration, which is where `INTERRUPTIBLE` removes the acceleration.
///
/// 0: MOVZ X0, #iters                   (built by mov64)
/// n: BLR  X1                           D63F0020
/// n: SUBS X0, X0, #1                   F1000400
/// n: B.NE -2                           54FFFFC1
/// n: SVC  #0                           D4000001
/// n: RET  X30                          D65F03C0    <- the callee, X1 points here
fn indirect_program(iterations: u64, padding: usize) -> (Vec<u32>, u64) {
    let mut code = a64::mov64(0, iterations);
    // The callee's address has to be known before the instructions that load
    // it are emitted, and those instructions change where the callee lands. So
    // pin the load at exactly two instructions -- MOVZ plus MOVK, which covers
    // every address CODE_BASE can reach here -- and the arithmetic closes.
    let loop_start = code.len() + 2;
    let callee_index = loop_start + 4 + padding;
    let callee = CODE_BASE + 4 * callee_index as u64;
    assert!(callee < (1 << 32), "the two-instruction load assumes a 32-bit address");
    code.push(a64::movz(1, callee as u16, 0));
    code.push(a64::movk(1, (callee >> 16) as u16, 1));
    assert_eq!(code.len(), loop_start);
    code.push(a64::blr(1));
    for _ in 0..padding {
        code.push(a64::add_imm(2, 2, 1));
    }
    code.push(a64::subs_imm(0, 0, 1));
    code.push(a64::b_cond(cond::NE, -(2 + padding as i32)));
    code.push(a64::svc(0));
    assert_eq!(code.len(), callee_index);
    code.push(a64::ret(30));
    // BLR, RET, SUBS, B.NE, and `padding` ADDs per iteration.
    (code, iterations * (4 + padding as u64))
}

/// The same shape with no indirect transfer at all: four instructions per
/// iteration, one direct conditional branch.
///
/// n: ADD  X2, X2, #1                   91000442
/// n: ADD  X3, X3, #1                   91000463
/// n: SUBS X0, X0, #1                   F1000400
/// n: B.NE -3                           54FFFFA1
/// n: SVC  #0                           D4000001
fn direct_program(iterations: u64) -> (Vec<u32>, u64) {
    let mut code = a64::mov64(0, iterations);
    code.push(a64::add_imm(2, 2, 1));
    code.push(a64::add_imm(3, 3, 1));
    code.push(a64::subs_imm(0, 0, 1));
    code.push(a64::b_cond(cond::NE, -3));
    code.push(a64::svc(0));
    (code, iterations * 4)
}

fn steady_state(code: Vec<u32>, optimizations: u32) -> Summary {
    let vm = Vm::new(
        code,
        VmOptions {
            optimizations,
            // Off: this measures dispatch, and cycle counting adds a compare
            // per block to both configurations for no benefit here.
            cycle_counting: false,
            ..VmOptions::default()
        },
    );
    measure(|| {
        vm.start(u64::MAX);
        let hr = vm.run_to_completion(64);
        assert_eq!(hr & HALT_DONE, HALT_DONE);
    })
}

#[test]
#[ignore = "measurement, not a test"]
fn cost_of_interruptible_dispatch() {
    const ITERATIONS: u64 = 100_000;

    // Three points, because the answer is a function of branch mix and a
    // single number would be a lie whichever one was picked.
    let (saturated, saturated_insns) = indirect_program(ITERATIONS, 0);
    let (mixed, mixed_insns) = indirect_program(ITERATIONS, 8);
    let (direct, direct_insns) = direct_program(ITERATIONS);

    let cases = [
        ("indirect 2-in-4", saturated, saturated_insns),
        ("indirect 2-in-12", mixed, mixed_insns),
        ("no indirect", direct, direct_insns),
    ];

    println!("n={N} per configuration, release, {ITERATIONS} iterations");
    for (name, code, insns) in cases {
        let all = steady_state(code.clone(), optimization::ALL_SAFE);
        let interruptible = steady_state(code.clone(), optimization::INTERRUPTIBLE);
        // `INTERRUPTIBLE` with `BlockLinking` cleared too: the only flag set in
        // which every runaway shape is stoppable by both mechanisms. Its cost
        // is driven by how long the average basic block is, not by branch mix,
        // so it is reported for every workload rather than folded into the
        // ratio above.
        let stoppable =
            steady_state(code, optimization::INTERRUPTIBLE & !optimization::BLOCK_LINKING);
        let ratio = interruptible.median.as_secs_f64() / all.median.as_secs_f64();
        let stoppable_ratio = stoppable.median.as_secs_f64() / all.median.as_secs_f64();
        println!(
            "{name:22} ALL_SAFE      {:6.3}-{:6.3} ms (median {:6.3}, {:7.1} Mguest-insn/s)",
            ms(all.min),
            ms(all.max),
            ms(all.median),
            all.rate(insns)
        );
        println!(
            "{name:22} INTERRUPTIBLE {:6.3}-{:6.3} ms (median {:6.3}, {:7.1} Mguest-insn/s)",
            ms(interruptible.min),
            ms(interruptible.max),
            ms(interruptible.median),
            interruptible.rate(insns)
        );
        println!(
            "{name:22} no-BlockLink  {:6.3}-{:6.3} ms (median {:6.3}, {:7.1} Mguest-insn/s)",
            ms(stoppable.min),
            ms(stoppable.max),
            ms(stoppable.median),
            stoppable.rate(insns)
        );
        println!(
            "{name:22} ratio         {ratio:.2}x INTERRUPTIBLE,              {stoppable_ratio:.2}x no-BlockLink"
        );
        if name != "no indirect" {
            // The ratio is a property of the mix; the per-transfer cost is a
            // property of the change, and it is the number that transfers to
            // another workload.
            let transfers = (ITERATIONS * 2) as f64;
            let extra_ns =
                (interruptible.median.as_secs_f64() - all.median.as_secs_f64()) * 1e9 / transfers;
            println!("{name:22} per indirect transfer {extra_ns:.2} ns");
        }
    }
}

/// A program of `BLOCKS` basic blocks, each three instructions and a branch to
/// the next. `B +1` is what forces a block boundary: a straight run of
/// arithmetic is one block however long it is.
fn many_blocks(blocks: usize) -> (Vec<u32>, u64) {
    let mut code = Vec::with_capacity(blocks * 4 + 1);
    for _ in 0..blocks {
        code.push(a64::add_imm(0, 0, 1));
        code.push(a64::add_imm(1, 1, 1));
        code.push(a64::add_imm(2, 2, 1));
        code.push(a64::b(1));
    }
    code.push(a64::svc(0));
    (code, blocks as u64 * 4)
}

#[test]
#[ignore = "measurement, not a test"]
fn cost_of_cold_translation() {
    // This is where `DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT` lands: the
    // `EnableWriting`/`DisableWriting` pair brackets `EmitBlock`
    // (`a64_emit_x64.cpp:72`), so it is one `VirtualProtect` pair per block
    // over the whole committed region. Run this with and without the
    // `w-xor-x` feature and compare.
    const BLOCKS: usize = 2_000;
    let (code, insns) = many_blocks(BLOCKS);

    let vm = Vm::new(
        code,
        VmOptions {
            cycle_counting: false,
            // 64 MiB, so the committed region grows to something a
            // whole-region VirtualProtect has to walk.
            code_cache_size: 64 << 20,
            ..VmOptions::default()
        },
    );

    let summary = measure(|| {
        // SAFETY: `vm.raw()` is live and not executing.
        unsafe { dynarmic_sys::od_jit_clear_cache(vm.raw()) };
        vm.start(u64::MAX);
        let hr = vm.run_to_completion(64);
        assert_eq!(hr & HALT_DONE, HALT_DONE);
    });

    println!(
        "cold translation, w-xor-x={}, n={N}: {BLOCKS} blocks / {insns} instructions, \
         {:.3}-{:.3} ms (median {:.3}, {:.3} Mguest-insn/s, {:.1} us/block)",
        cfg!(feature = "w-xor-x"),
        ms(summary.min),
        ms(summary.max),
        ms(summary.median),
        summary.rate(insns),
        summary.median.as_secs_f64() * 1e6 / BLOCKS as f64,
    );
}
