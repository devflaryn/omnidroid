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
//! 1. **What does identity mapping actually buy, here, on this guest?** D4's spike measured 13.2x
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
use omni_cpu::dynarmic::{DynarmicOptions, MemoryPathOverrides};
use omni_cpu::{ExitReason, GuestCpu, RunLimit};

/// Samples per configuration. Odd, so the median is an observation rather than an average of two.
const N: usize = 31;

/// Serializes every measurement in this binary.
///
/// Two reasons, and the second is not optional. Timings taken while a sibling benchmark is running
/// measure the scheduler; and `the_commit_charge_of_a_guest_thread` reads
/// `process_commit_charge`, which is **process-global**, so a concurrent benchmark creating jits
/// moves it underneath the measurement. Unserialized it read 34.057 MiB/thread against 24.562
/// serialized — a 39% error in a figure that is supposed to say what a guest thread costs. This is
/// the same trap Task 1 hit with `PrivateUsage`, and the same fix.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
    let _serial = serialized();
    // The per-slice callback invariant is off here because this benchmark's whole purpose is to
    // *run* the degraded configuration and time it; with the invariant on, the first slice raises
    // `DegradedMemoryPath` instead, which is asserted in `tests/identity.rs`.
    let guest = Guest::with_options(DynarmicOptions {
        assert_callback_free_slices: false,
        ..Default::default()
    });
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
            .create_misconfigured_thread(MemoryPathOverrides {
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
        "  ratio           : {:.2}x  (D4's spike measured 13.2x against bare-stub callbacks)",
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
    let _serial = serialized();
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

/// **What a guest thread really costs in commit charge**, as against what [`GuestCpu::cost`]
/// reports.
///
/// `cost()` reports the TLS block and says plainly that it omits dynarmic's code cache, because
/// this pin exposes no way to read the cache's committed high-water mark. This measures the whole
/// thing from the outside, which is the only way to say how large the omission is.
///
/// The counter is process-global, so the window is kept as narrow as the work allows and the figure
/// is reported as a range rather than a point. Task 1 spent three rounds learning that lesson about
/// this exact counter.
#[test]
#[ignore = "measurement, not a test"]
fn the_commit_charge_of_a_guest_thread() {
    let _serial = serialized();
    const THREADS: usize = 8;
    let guest = Guest::with_options(DynarmicOptions {
        max_threads: THREADS as u32,
        ..Default::default()
    });
    let entry = guest.load(&memory_loop(guest.data, 10_000));
    guest.write_u64(guest.data, 1);
    let sentinel = guest.code + harness::CODE_BYTES - 4;

    let baseline = omni_mem::process_commit_charge().expect("commit charge");
    let mut threads = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        threads.push(guest.backend.create_thread_with_tls().expect("a guest thread"));
    }
    let created = omni_mem::process_commit_charge().expect("commit charge");

    // Now make each one translate something, since the cache commits as code is emitted.
    for cpu in &mut threads {
        cpu.set_return_sentinel(sentinel).expect("sentinel");
        cpu.set_x(x(30), sentinel as u64);
        cpu.run(entry, RunLimit::Unlimited).expect("the loop runs");
    }
    let warm = omni_mem::process_commit_charge().expect("commit charge");

    let reported: usize = threads.iter().map(|c| c.cost().total()).sum();
    drop(threads);
    let after_drop = omni_mem::process_commit_charge().expect("commit charge");

    println!("
== per-guest-thread commit charge (n = 1 run, {THREADS} threads) ==");
    println!("  after creating {THREADS} contexts : {:+} bytes ({:.3} MiB), {:.3} MiB/thread",
        created as i64 - baseline as i64,
        (created - baseline) as f64 / 1048576.0,
        (created - baseline) as f64 / 1048576.0 / THREADS as f64);
    println!("  after each has translated     : {:+} bytes ({:.3} MiB), {:.3} MiB/thread",
        warm as i64 - baseline as i64,
        (warm - baseline) as f64 / 1048576.0,
        (warm - baseline) as f64 / 1048576.0 / THREADS as f64);
    println!("  GuestCpu::cost() reports      : {reported} bytes total, {} per thread",
        reported / THREADS);
    println!("  after dropping them           : {:+} bytes", after_drop as i64 - baseline as i64);
    println!(
        "  The gap between the middle two lines is dynarmic's per-jit state, which this pin gives \
         no way to read. D5 measured 20-35 MiB/thread against the 128 MiB default cache; this \
         backend defaults to 8 MiB."
    );

    // Does the code cache size actually drive it? D5's risk 2 -- per-thread memory -- is recorded
    // as a consequence of the code cache, and the obvious mitigation is a smaller one. This is the
    // measurement that says whether that mitigation works.
    println!("  per-thread charge against code_cache_size (4 contexts each, 1 run):");
    for cache in [8u64 << 20, 32 << 20, 128 << 20] {
        let guest = Guest::with_options(DynarmicOptions {
            max_threads: 4,
            code_cache_size: cache,
            ..Default::default()
        });
        let before = omni_mem::process_commit_charge().expect("commit charge");
        let held: Vec<_> =
            (0..4).map(|_| guest.backend.create_thread_with_tls().expect("a thread")).collect();
        let after = omni_mem::process_commit_charge().expect("commit charge");
        println!(
            "    code_cache_size {:>4} MiB : {:7.3} MiB/thread",
            cache >> 20,
            (after - before) as f64 / 1048576.0 / 4.0
        );
        drop(held);
    }
    println!(
        "  If those figures do not track the cache size, the per-thread cost is not the cache. \
         `A64EmitX64` holds a `std::array<FastDispatchEntry, 0x100000>` -- a flat 16 MiB per jit, \
         allocated and zeroed in the constructor whether or not the FastDispatch optimization is \
         enabled, and this backend disables it.
"
    );
}

/// **What the sliced run loop costs.** The watchdog returns to the dispatcher every
/// `SLICE_INSTRUCTIONS`; a short slice makes that visible and is the control.
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_slicing_the_run_loop() {
    let _serial = serialized();
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

/// **What the per-slice callback invariant costs.**
///
/// Task 4 added it because the D4 startup assertion structurally cannot see a memory path that
/// degrades *after* it has passed. The check is two reads of a non-atomic counter on the jit's own
/// thread, around a slice that is [`omni_cpu::run::SLICE_INSTRUCTIONS`] guest instructions — so the
/// claim is that it is free, and a claim like that should be a number.
///
/// Two measurements, because the end-to-end one alone would be indistinguishable from noise and
/// therefore would not say what the check costs, only that the workload is long:
///
/// 1. the whole workload, invariant on against off, which is the figure that matters;
/// 2. the cost of one counter read on its own, which is what multiplies by the slice count.
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_the_per_slice_callback_invariant() {
    let _serial = serialized();
    let mut rows = Vec::new();
    for armed in [false, true] {
        let guest = Guest::with_options(DynarmicOptions {
            assert_callback_free_slices: armed,
            ..Default::default()
        });
        let entry = guest.load(&memory_loop(guest.data, ITERATIONS));
        guest.write_u64(guest.data, 1);
        let (mut cpu, sentinel) = guest.thread();
        rows.push(measure(|| {
            cpu.set_x(x(30), sentinel as u64);
            let exit = cpu.run(entry, RunLimit::Unlimited).expect("the loop runs");
            assert_eq!(exit, ExitReason::Returned { pc: sentinel });
        }));
    }

    // The cost of the read itself. `slow_path_entries` is `od_jit_slow_path_total`, one load
    // across the FFI boundary; the loop is `black_box`ed so it is not optimized away.
    const READS: u64 = 10_000_000;
    let guest = Guest::new();
    let (cpu, _) = guest.thread();
    let mut read_samples = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        let mut acc = 0u64;
        for _ in 0..READS {
            acc = acc.wrapping_add(std::hint::black_box(cpu.slow_path_entries()));
        }
        std::hint::black_box(acc);
        read_samples.push(t.elapsed());
    }
    let read = Summary::of(read_samples);
    let ns_per_read = read.median.as_secs_f64() * 1e9 / READS as f64;

    let slices = LOOP_INSTRUCTIONS.div_ceil(omni_cpu::run::SLICE_INSTRUCTIONS);
    println!("\n== the per-slice callback invariant (n = {N} per configuration) ==");
    println!(
        "{LOOP_INSTRUCTIONS} guest instructions, slice = {}, so {slices} slices and {} counter \
         reads per run",
        omni_cpu::run::SLICE_INSTRUCTIONS,
        slices * 2
    );
    for (name, summary) in [("disarmed", &rows[0]), ("armed", &rows[1])] {
        println!(
            "  {name:9} : {:7.3} ms median  [{:7.3} .. {:7.3}]",
            ms(summary.median),
            ms(summary.min),
            ms(summary.max)
        );
    }
    println!(
        "  ratio     : {:.4}x",
        rows[1].median.as_secs_f64() / rows[0].median.as_secs_f64()
    );
    println!(
        "  one counter read: {ns_per_read:.3} ns (median of {N} runs of {READS} reads), so \
         {:.3} ns per slice and {:.6} ms across the whole run",
        ns_per_read * 2.0,
        ns_per_read * 2.0 * slices as f64 / 1e6
    );
}

/// `x0` = the word, `x9` = iterations: one `LDAXR`/`ADD`/`STLXR`/`CBNZ` increment per iteration,
/// the shape LLVM emits for a C++ `fetch_add`. `x10` counts failed store-exclusives.
fn exclusive_increment_loop() -> Vec<u32> {
    let mut p = vec![movz(10, 0, 0)];
    let top = p.len();
    p.push(ldaxr(2, 0));
    p.push(add_imm(2, 2, 1));
    p.push(stlxr(3, 2, 0));
    let cbnz_at = p.len();
    p.push(0);
    p.push(subs_imm(9, 9, 1));
    let here = p.len();
    p.push(b_cond(1, top as i32 - here as i32));
    p.push(ret(30));
    let retry = p.len();
    p.push(add_imm(10, 10, 1));
    let here = p.len();
    p.push(b(top as i32 - here as i32));
    p[cbnz_at] = cbnz_w(3, retry as i32 - cbnz_at as i32);
    p
}

/// **What one guest atomic increment costs under each exclusive monitor**, and how that scales
/// with the monitor's size and with threads -- the measurement H1 of the world performance work
/// (`docs/research/perf-world.md`) starts from.
///
/// The global monitor's store-exclusive scans every slot the monitor was sized for, inline and
/// unrolled, under one process-wide spin lock; the runtime sizes it from `max_threads`, which
/// `23530d1` raised from 64 to 256. So the rows are the monitor at 1, 64 and 256 slots, and
/// value-compare at 256 threads, each with one thread on a private word, eight threads on eight
/// private words (each on its own cache lines), and eight threads on one shared word.
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_a_guest_atomic_increment() {
    use omni_cpu::dynarmic::ExclusiveMonitor;
    let _serial = serialized();
    const SAMPLES: usize = 7;
    const ONE: u64 = 1_000_000;
    const EACH: u64 = 200_000;
    const THREADS: usize = 8;
    println!("\n== a guest LDAXR/ADD/STLXR/CBNZ increment (median of {SAMPLES}; ns per increment, wall) ==");
    for (label, monitor, max_threads) in [
        ("global, 1 slot   ", ExclusiveMonitor::Global, 1u32),
        ("global, 64 slots ", ExclusiveMonitor::Global, 64),
        ("global, 256 slots", ExclusiveMonitor::Global, 256),
        ("value-compare    ", ExclusiveMonitor::ValueCompare, 256),
    ] {
        let threads_here = if max_threads < THREADS as u32 { 1 } else { THREADS };
        let guest = Guest::with_options(DynarmicOptions {
            max_threads,
            exclusive_monitor: monitor,
            ..Default::default()
        });
        let entry = guest.load(&exclusive_increment_loop());
        let sentinel = guest.code + harness::CODE_BYTES - 4;
        let mut cpus: Vec<_> = (0..threads_here)
            .map(|_| {
                let mut cpu = guest.backend.create_thread_with_tls().expect("a guest thread");
                cpu.set_return_sentinel(sentinel).expect("arm the sentinel");
                cpu
            })
            .collect();
        // (label, threads, shared word?, iterations each)
        let mut cells = vec![("1 thread, private word", 1usize, false, ONE)];
        if threads_here == THREADS {
            cells.push(("8 threads, private words", THREADS, false, EACH));
            cells.push(("8 threads, one shared word", THREADS, true, EACH));
        }
        for (cell, n, shared, each) in cells {
            let mut samples = Vec::with_capacity(SAMPLES);
            let mut retries = 0u64;
            for round in 0..=SAMPLES {
                for i in 0..n {
                    guest.write_u64(guest.data + if shared { 0 } else { i * 128 }, 0);
                }
                let start = std::sync::Barrier::new(n + 1);
                let (elapsed, failed) = std::thread::scope(|scope| {
                    let handles: Vec<_> = cpus[..n]
                        .iter_mut()
                        .enumerate()
                        .map(|(i, cpu)| {
                            let start = &start;
                            let word = guest.data + if shared { 0 } else { i * 128 };
                            scope.spawn(move || {
                                cpu.set_x(x(0), word as u64);
                                cpu.set_x(x(9), each);
                                cpu.set_x(x(30), sentinel as u64);
                                start.wait();
                                let exit = cpu.run(entry, RunLimit::Unlimited).expect("runs");
                                assert_eq!(exit, ExitReason::Returned { pc: sentinel });
                                cpu.x(x(10))
                            })
                        })
                        .collect();
                    start.wait();
                    let t = Instant::now();
                    let failed: u64 = handles.into_iter().map(|h| h.join().expect("joined")).sum();
                    (t.elapsed(), failed)
                });
                let total: u64 = if shared {
                    guest.read_u64(guest.data)
                } else {
                    (0..n).map(|i| guest.read_u64(guest.data + i * 128)).sum()
                };
                assert_eq!(total, n as u64 * each, "{label} {cell}: increments lost");
                if round > 0 {
                    // Round 0 warms the translation, as `measure` does.
                    samples.push(elapsed);
                    retries += failed;
                }
            }
            let summary = Summary::of(samples);
            let ns = summary.median.as_secs_f64() * 1e9 / (n as u64 * each) as f64;
            println!(
                "  {label} | {cell:27} : {ns:8.1} ns/increment  [{:7.1} .. {:7.1}]  retries/increment {:.3}",
                summary.min.as_secs_f64() * 1e9 / (n as u64 * each) as f64,
                summary.max.as_secs_f64() * 1e9 / (n as u64 * each) as f64,
                retries as f64 / (SAMPLES as u64 * n as u64 * each) as f64
            );
        }
    }
}

/// `BL offset` -- `1 00101 imm26`. Offset in instructions.
const fn bl_rel(offset_insns: i64) -> u32 {
    0x9400_0000 | ((offset_insns as u32) & 0x03FF_FFFF)
}

/// **What a guest call and return cost as the number of distinct blocks grows**, under the flag set
/// the runtime uses (`INTERRUPTIBLE`: every `RET` returns to the dispatcher, which calls
/// `GetCurrentBlockThunk` and looks the next block up in a `tsl::robin_map`) and under dynarmic's
/// default (`ALL_SAFE`: a return-stack buffer predicts the `RET`, and a fast-dispatch table serves
/// the misses from emitted code). H2 of the world performance work.
///
/// The guest is `K` call sites, each a `BL` to its own two-instruction function (`add; ret`), in a
/// loop: `2K` blocks, one direct transfer and one indirect transfer per call. `K` runs from a
/// microbenchmark's size to a world's (a busy engine thread translates hundreds of thousands of
/// blocks), with a 128 MiB code cache so that no configuration evicts.
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_a_call_and_return_as_the_block_map_grows() {
    use omni_cpu::dynarmic::DynarmicBackend;
    use omni_mem::{CommitPolicy, GuestSpace, Placement, Protection};
    let _serial = serialized();
    const SAMPLES: usize = 5;
    println!("\n== one BL + RET to a distinct function, ns per call (median of {SAMPLES}) ==");
    for k in [64usize, 1024, 16_384, 131_072] {
        let calls_per_round: u64 = 4_000_000;
        let loops = (calls_per_round / k as u64).max(1);
        let mut row = Vec::new();
        for (label, interruptible) in [("INTERRUPTIBLE", true), ("ALL_SAFE", false)] {
            let space = std::sync::Arc::new(GuestSpace::new().expect("a guest space"));
            let bytes = (k * 4 + 64 + k * 8 + 0xFFFF) & !0xFFFF;
            let code = space
                .map_anonymous(
                    Placement::Anywhere { align: space.page_size() },
                    bytes,
                    Protection::ReadWrite,
                    CommitPolicy::Eager,
                )
                .expect("a code region");
            // Caller: x9 = loops; mov x20, x30; top: BL f_0 .. BL f_{k-1}; subs x9; b.ne top;
            // mov x30, x20; ret. Functions follow, 8 bytes each.
            let mut program = vec![mov_reg(20, 30)];
            let top = program.len();
            let functions_at = 1 + k + 4; // words
            for i in 0..k {
                let here = program.len() as i64;
                program.push(bl_rel((functions_at + 2 * i) as i64 - here));
            }
            program.push(subs_imm(9, 9, 1));
            let here = program.len();
            program.push(b_cond(1, top as i32 - here as i32));
            program.push(mov_reg(30, 20));
            program.push(ret(30));
            assert_eq!(program.len(), functions_at);
            for _ in 0..k {
                program.push(add_imm(2, 2, 1));
                program.push(ret(30));
            }
            let ptr = space.ptr(code, program.len() * 4).expect("a host pointer");
            // SAFETY: `ptr` is a committed, writable range of exactly this length in this space.
            unsafe { core::ptr::copy_nonoverlapping(program.as_ptr(), ptr.cast::<u32>(), program.len()) };
            space.protect(code, bytes, Protection::ReadExecute).expect("executable");
            let backend = DynarmicBackend::new(
                std::sync::Arc::clone(&space),
                DynarmicOptions { code_cache_size: 128 << 20, interruptible, ..Default::default() },
            )
            .expect("a backend");
            let mut cpu = backend.create_thread_with_tls().expect("a context");
            let sentinel = code + bytes - 4;
            cpu.set_return_sentinel(sentinel).expect("sentinel");
            let mut samples = Vec::new();
            for round in 0..=SAMPLES {
                cpu.set_x(x(30), sentinel as u64);
                cpu.set_x(x(9), loops);
                let t = Instant::now();
                let exit = cpu.run(code, RunLimit::Unlimited).expect("runs");
                assert_eq!(exit, ExitReason::Returned { pc: sentinel });
                if round > 0 {
                    samples.push(t.elapsed());
                }
            }
            let summary = Summary::of(samples);
            let ns = summary.median.as_secs_f64() * 1e9 / (loops * k as u64) as f64;
            row.push(format!("{label} {ns:6.2}"));
        }
        println!("  {:>7} call sites ({:>7} blocks): {}", k, 2 * k, row.join(" | "));
    }
}
