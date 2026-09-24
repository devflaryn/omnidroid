//! **The sampler behind `OMNI_PERF`, shown seeing cases whose answer is known**
//! (`docs/VERIFICATION.md` entry 19: an instrument that returns a default when it cannot see is
//! indistinguishable from one that saw the default).
//!
//! Three real guest threads through a real boundary: two contend on one word with
//! `LDAXR`/`STLXR` increments under the global exclusive monitor, and one runs a register loop
//! with no exclusive access at all. The sampler must put the first two mostly at the monitor and
//! the third in translated code and nowhere near the monitor. If `mon` never lit up here, a world
//! reading of `mon 0%` would mean nothing.
//!
//! On macOS (arm64) `mon` is not recognised (its heuristic reads x86-64 encodings), so there the
//! contending threads are asserted to be in translated code -- which still shows the sampler
//! finding a code cache, the thing a world's `jit` share depends on.

#![cfg(any(target_arch = "x86_64", all(target_os = "macos", target_arch = "aarch64")))]

mod harness;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::{GuestCpu, RunLimit};

/// `LDAXR Xt, [Xn]`.
const fn ldaxr(rt: u32, rn: u32) -> u32 {
    0xC85F_FC00 | (rn << 5) | rt
}
/// `STLXR Ws, Xt, [Xn]`.
const fn stlxr(rs: u32, rt: u32, rn: u32) -> u32 {
    0xC800_FC00 | (rs << 16) | (rn << 5) | rt
}
/// `CBNZ Wt, offset` (instructions).
const fn cbnz_w(rt: u32, offset: i32) -> u32 {
    0x3500_0000 | (((offset as u32) & 0x7FFFF) << 5) | rt
}
/// `CBZ Xt, offset` (instructions).
const fn cbz_x(rt: u32, offset: i32) -> u32 {
    0xB400_0000 | (((offset as u32) & 0x7FFFF) << 5) | rt
}

/// `x0` = the shared word, `x6` = the stop word: exclusive increments until the stop word is set.
fn exclusive_until_stopped() -> Vec<u32> {
    let mut p = Vec::new();
    let top = p.len() as i32;
    p.push(ldaxr(2, 0));
    p.push(add_imm(2, 2, 1));
    p.push(stlxr(3, 2, 0));
    let here = p.len() as i32;
    p.push(cbnz_w(3, top - here));
    p.push(ldr_imm(5, 6, 0));
    let here = p.len() as i32;
    p.push(cbz_x(5, top - here));
    p.push(ret(30));
    p
}

/// `x6` = the stop word: register arithmetic until the stop word is set.
fn registers_until_stopped() -> Vec<u32> {
    let mut p = Vec::new();
    let top = p.len() as i32;
    p.push(add_imm(2, 2, 1));
    p.push(add_reg(3, 3, 2));
    p.push(add_reg(4, 4, 3));
    p.push(ldr_imm(5, 6, 0));
    let here = p.len() as i32;
    p.push(cbz_x(5, top - here));
    p.push(ret(30));
    p
}

#[test]
fn the_sampler_puts_monitor_contention_at_the_monitor_and_register_code_in_the_jit() {
    // Every host this file builds for has a sampler (`omni_platform::sampler`: Windows, Linux
    // x86-64, macOS arm64); one that could not open a thread would leave every row empty, so it
    // fails here by name rather than passing on nothing.
    if let Err(error) = omni_platform::sampler::HostThread::current() {
        panic!("this host's sampler cannot open a thread: {error}");
    }
    omni_android::perf::keep_thread_records();
    let guest = Guest::new();
    let exclusive = guest.load(&exclusive_until_stopped());
    let registers = guest.load(&registers_until_stopped());
    let boundary = guest.boundary(8).finish();
    let word = guest.data;
    let stop = guest.data + 64;
    guest.write_u64(word, 0);
    guest.write_u64(stop, 0);

    let started = Arc::new(AtomicU32::new(0));
    let mut ids = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for entry in [exclusive, exclusive, registers] {
            let mut cpu = guest.thread(&boundary);
            cpu.set_x(x(0), word as u64);
            cpu.set_x(x(6), stop as u64);
            let boundary = Arc::clone(&boundary);
            let started = Arc::clone(&started);
            let (tx, rx) = std::sync::mpsc::channel();
            handles.push(scope.spawn(move || {
                tx.send(omni_platform::sampler::HostThread::current().expect("a handle").os_id())
                    .expect("sent");
                started.fetch_add(1, Ordering::Relaxed);
                boundary.run(&mut cpu, entry, RunLimit::Unlimited).expect("the loop runs")
            }));
            ids.push(rx.recv().expect("its thread id"));
        }
        while started.load(Ordering::Relaxed) < 3 {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(100));
        // The sampler's own cost: this thread's CPU over the profile, as the PERF block reports it.
        let me = omni_platform::sampler::HostThread::current().expect("a handle to this thread");
        let (cpu_before, wall) = (me.cpu_time().expect("its CPU"), std::time::Instant::now());
        let profiles = omni_android::perf::profile(&boundary, Duration::from_millis(800), 200);
        let share = me.cpu_time().expect("its CPU").saturating_sub(cpu_before).as_secs_f64()
            / wall.elapsed().as_secs_f64();
        println!("sampler cpu {:.2}% at 200 Hz over 3 busy threads", 100.0 * share);
        assert!(share < 0.10, "the sampler used {:.1}% of a core", 100.0 * share);
        guest.write_u64(stop, 1);
        for handle in handles {
            handle.join().expect("a guest thread panicked");
        }

        let of = |id: u32| {
            profiles
                .iter()
                .find(|p| p.os_id == id)
                .unwrap_or_else(|| panic!("no profile for thread {id}: {profiles:?}"))
                .clone()
        };
        for id in &ids[..2] {
            let p = of(*id);
            let ran = p.ticks - p.idle;
            println!("contending thread {id}: {p:?}");
            assert!(ran > 50, "thread {id} was sampled only {ran} times: {p:?}");
            #[cfg(target_arch = "x86_64")]
            assert!(
                p.monitor * 2 >= ran,
                "a thread contending on the global monitor was put at the monitor in only {} of {ran} \
                 samples: {p:?}",
                p.monitor
            );
            #[cfg(not(target_arch = "x86_64"))]
            assert!(
                p.jit * 2 >= ran,
                "a thread running an exclusive-increment loop was in translated code in only {} of \
                 {ran} samples: {p:?}",
                p.jit
            );
            assert_eq!(p.handler, 0, "no handler was ever entered: {p:?}");
        }
        let p = of(ids[2]);
        let ran = p.ticks - p.idle;
        println!("register thread {}: {p:?}", ids[2]);
        assert!(ran > 50, "the register thread was sampled only {ran} times: {p:?}");
        assert!(p.jit * 10 >= ran * 8, "a register loop was in translated code in only {} of {ran}: {p:?}", p.jit);
        assert!(p.monitor * 50 <= ran, "a loop with no exclusive access was put at the monitor: {p:?}");
    });
    assert!(guest.read_u64(word) > 0, "the contending threads made progress");
}

fn noop(c: &mut omni_android::ImportCall<'_, '_>) -> omni_android::AbiResult<()> {
    c.ret().u64(0);
    Ok(())
}

/// **What an import crossing costs with the census on and off, on one thread and on eight** --
/// H5 of the world performance work (`docs/research/perf-world.md`). The census, which the gate
/// keeps on for the whole session, used to charge every crossing to a per-symbol counter shared
/// by every thread and to two process-wide "last call" words -- cache lines every core writes
/// (MEASURED here: 518-614 ns a crossing with eight threads on one symbol, against 48-50 with the
/// census off). It now writes only the crossing thread's own record (`Boundary::start_census`),
/// and this is the measurement that says whether that holds.
///
/// `cargo test -p omni-android --release --test perf -- --ignored --nocapture`
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_an_import_crossing_with_the_census_on_and_off() {
    const EACH: u64 = 2_000_000;
    const THREADS: usize = 8;
    let guest = Guest::new();
    let builder = guest.boundary(16);
    let thunks: Vec<_> = (0..THREADS)
        .map(|i| builder.bind_inline(&format!("noop{i}"), noop).expect("bind"))
        .collect();
    let boundary = builder.finish();
    // One loop per thunk, so a thread can cross its own symbol or everyone can cross one.
    let entries: Vec<_> = thunks
        .iter()
        .map(|&thunk| {
            let at = guest.next_entry();
            let mut asm = harness::Asm::at(at);
            asm.push(mov_reg(20, 30));
            let top = asm.pc();
            asm.bl(thunk);
            asm.push(subs_imm(1, 1, 1));
            let here = asm.pc();
            asm.push(b_cond(1, (top as i64 - here as i64) as i32 / 4));
            asm.push(mov_reg(30, 20));
            asm.push(ret(30));
            let entry = guest.load(asm.words());
            assert_eq!(entry, at);
            entry
        })
        .collect();
    println!("
== one inline import crossing, ns per crossing per thread (median of 5) ==");
    for (label, threads, shared) in
        [("1 thread", 1usize, true), ("8 threads, one symbol", THREADS, true), ("8 threads, 8 symbols", THREADS, false)]
    {
        for census in [false, true] {
            if census {
                boundary.start_census();
            } else {
                boundary.stop_census();
            }
            let mut cpus: Vec<_> = (0..threads).map(|_| guest.thread(&boundary)).collect();
            let mut samples = Vec::new();
            for round in 0..6 {
                for cpu in cpus.iter_mut() {
                    guest.rearm(cpu, &boundary);
                    cpu.set_x(x(1), EACH);
                }
                let start = std::sync::Barrier::new(threads + 1);
                let elapsed = std::thread::scope(|scope| {
                    let handles: Vec<_> = cpus
                        .iter_mut()
                        .enumerate()
                        .map(|(i, cpu)| {
                            let boundary = Arc::clone(&boundary);
                            let start = &start;
                            let entry = if shared { entries[0] } else { entries[i] };
                            scope.spawn(move || {
                                start.wait();
                                boundary.run(cpu, entry, RunLimit::Unlimited).expect("runs");
                            })
                        })
                        .collect();
                    start.wait();
                    let t = std::time::Instant::now();
                    for h in handles {
                        h.join().expect("joined");
                    }
                    t.elapsed()
                });
                if round > 0 {
                    samples.push(elapsed);
                }
            }
            samples.sort();
            let median = samples[samples.len() / 2];
            println!(
                "  {label:22}, census {:3} : {:6.1} ns per crossing per thread",
                if census { "on" } else { "off" },
                median.as_secs_f64() * 1e9 / EACH as f64
            );
        }
    }
    boundary.stop_census();
}

/// **The same measurement through a real bionic import**, `pthread_self`, on one thread and on
/// eight calling it at once. The noop above crosses the boundary and nothing else; a bionic
/// handler also finds its instance (`bionic::active`) on every call, which is the rest of what a
/// world's ~5 M crossings a second pay.
///
/// `cargo test -p omni-android --release --test perf -- --ignored --nocapture`
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_a_bionic_import_crossing_with_the_census_on_and_off() {
    use omni_android::bionic::Bionic;
    use std::sync::Barrier;
    const EACH: u64 = 2_000_000;
    const ROUNDS: usize = 6;
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    bionic.set_log_to_stderr(false);
    let builder = guest.boundary(256);
    bionic.bind_into(&builder).expect("bind every handler");
    let boundary = builder.finish();
    let thunk = boundary.slot_named("pthread_self").expect("pthread_self is bound").address;
    let entry = {
        let at = guest.next_entry();
        let mut asm = harness::Asm::at(at);
        asm.push(mov_reg(20, 30));
        let top = asm.pc();
        asm.bl(thunk);
        asm.push(subs_imm(1, 1, 1));
        let here = asm.pc();
        asm.push(b_cond(1, (top as i64 - here as i64) as i32 / 4));
        asm.push(mov_reg(30, 20));
        asm.push(ret(30));
        guest.load(asm.words())
    };
    println!("
== one bionic import crossing (pthread_self), ns per crossing per thread (median of 5) ==");
    for threads in [1usize, 8] {
        // Workers live for both census states, so each attaches to the instance once.
        let barrier = Barrier::new(threads + 1);
        // `Guest` is not `Sync`; the loop touches no stack, so every thread may share its top.
        let (stack_top, sentinel) = (guest.stack_top, boundary.sentinel());
        let cpus: Vec<_> = (0..threads).map(|_| guest.thread(&boundary)).collect();
        std::thread::scope(|scope| {
            for mut cpu in cpus {
                let (bionic, boundary, barrier) = (&bionic, &boundary, &barrier);
                scope.spawn(move || {
                    let _active = bionic.activate().expect("a thread block");
                    for _ in 0..2 * ROUNDS {
                        cpu.set_sp(stack_top);
                        cpu.set_x(x(30), sentinel as u64);
                        cpu.set_x(x(1), EACH);
                        barrier.wait();
                        boundary.run(&mut cpu, entry, RunLimit::Unlimited).expect("runs");
                        barrier.wait();
                    }
                });
            }
            for census in [false, true] {
                if census {
                    boundary.start_census();
                } else {
                    boundary.stop_census();
                }
                let mut samples = Vec::new();
                for round in 0..ROUNDS {
                    barrier.wait();
                    let t = std::time::Instant::now();
                    barrier.wait();
                    if round > 0 {
                        samples.push(t.elapsed());
                    }
                }
                samples.sort();
                let median = samples[samples.len() / 2];
                println!(
                    "  {:22}, census {:3} : {:6.1} ns per crossing per thread",
                    format!("{threads} thread(s)"),
                    if census { "on" } else { "off" },
                    median.as_secs_f64() * 1e9 / EACH as f64
                );
            }
        });
    }
    boundary.stop_census();
}
