//! **Every A64 decoder entry, executed.** The systematic version of `hostile.rs`'s random-word
//! fuzzer, and the reason it exists.
//!
//! A random 32-bit word lands in a given decoder entry with probability 2^-(fixed bits), so the
//! fuzzer reaches the common encodings quickly and a narrow one -- `CMHS D` has 21 fixed bits --
//! only after hundreds of thousands of trials: it took 158,510 to find that `CMHS D` terminated the
//! process on an arm64 host (patch 0006). A reachability study done by reading the frontend had
//! missed it too, because `CMHS` reaches `VectorMaxU64` through a helper in the IR emitter rather
//! than directly. Reading and sampling were both incomplete; enumerating is not.
//!
//! So: every `INST(...)` line of the pin's `frontend/A64/decoder/a64.inc` -- active entries and the
//! commented-out ones, which must reach the interpreter fallback -- is instantiated
//! [`VARIANTS`] times with its variable fields filled pseudo-randomly (deterministically), and each
//! instance is executed as `<word> ; SVC #0` with junk in every register. The table is used only as
//! a list of encodings to feed; nothing here takes an expected *result* from it.
//!
//! Each run is a child process. A child that dies is restarted after the word it died on, so one
//! run reports **every** word that takes the process down, not just the first.

mod harness;

use dynarmic_sys::*;
use harness::a64;
use harness::{Vm, VmOptions, CODE_BASE};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

/// Instances per decoder entry. `OD_SWEEP_VARIANTS` overrides it for a deeper one-off run.
const VARIANTS: usize = 48;

fn variants() -> usize {
    std::env::var("OD_SWEEP_VARIANTS").ok().and_then(|v| v.parse().ok()).unwrap_or(VARIANTS)
}

/// The pin's decoder table.
const TABLE: &str = include_str!("../vendor/dynarmic/src/dynarmic/frontend/A64/decoder/a64.inc");

/// `(name, pattern)` for every `INST(...)` line, commented out or not.
fn entries() -> Vec<(String, String)> {
    TABLE
        .lines()
        .filter_map(|line| {
            let line = line.trim_start_matches("//");
            let rest = line.strip_prefix("INST(")?;
            let name = rest.split(',').next()?.trim().to_string();
            let pattern = rest.rsplit('"').nth(1)?.to_string();
            (pattern.len() == 32).then_some((name, pattern))
        })
        .collect()
}

/// Deterministic SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Word pairs per pair sweep; `OD_SWEEP_PAIRS` overrides it.
const PAIRS: usize = 100_000;

/// The pair sweep's programs: `MOVZ Xr, #imm ; A ; B` for random corpus words `A` and `B`, so
/// that entries meet each other in one block -- and meet a constant the IR optimizer can propagate
/// into their operands, which reaches register-allocator paths a lone word with junk registers
/// never does.
fn pair_corpus() -> Vec<(String, Vec<u32>)> {
    let singles = corpus();
    let n = std::env::var("OD_SWEEP_PAIRS").ok().and_then(|v| v.parse().ok()).unwrap_or(PAIRS);
    let mut rng = Rng(0x0BA1_DA7A);
    (0..n)
        .map(|_| {
            let (na, a) = &singles[(rng.next() % singles.len() as u64) as usize];
            let (nb, b) = &singles[(rng.next() % singles.len() as u64) as usize];
            let r = rng.next();
            let movz = a64::movz((r % 31) as u32, (r >> 8) as u16, ((r >> 24) % 4) as u32);
            (format!("MOVZ {movz:08X} ; {na} {:08X} ; {nb} {:08X}", a[0], b[0]), vec![movz, a[0], b[0]])
        })
        .collect()
}

/// Every single word the sweep executes, in order, each as a one-word program.
fn corpus() -> Vec<(String, Vec<u32>)> {
    let mut rng = Rng(0x5EED_0F_A64_u64);
    let mut out = Vec::new();
    for (name, pattern) in entries() {
        for _ in 0..variants() {
            let random = rng.next() as u32;
            let mut word = 0u32;
            for (i, c) in pattern.chars().enumerate() {
                let bit = 31 - i;
                let value = match c {
                    '0' => 0,
                    '1' => 1,
                    _ => (random >> bit) & 1,
                };
                word |= value << bit;
            }
            out.push((name.clone(), vec![word]));
        }
    }
    out
}

/// The child: execute the corpus from `start`, printing each index before running it, so the
/// parent knows which word a death happened on.
fn run_from(corpus: &[(String, Vec<u32>)], start: usize, fastmem: bool) {
    let vm = Vm::new(
        vec![a64::svc(0)],
        VmOptions {
            fastmem,
            // The configuration omni-cpu runs: exclusives inline whenever fastmem is on.
            fastmem_exclusive: fastmem,
            cycle_counting: true,
            optimizations: optimization::INTERRUPTIBLE,
            ..VmOptions::default()
        },
    );
    let jit = vm.raw();
    let mut rng = Rng(start as u64);
    // A word that wedges the host thread is a finding too: a watchdog ends the child if the index
    // stops moving, and the parent files it under the word it stopped on.
    static PROGRESS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    std::thread::spawn(|| {
        let mut seen = usize::MAX;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let now = PROGRESS.load(std::sync::atomic::Ordering::Relaxed);
            if now == seen {
                println!("wedged");
                std::process::exit(9);
            }
            seen = now;
        }
    });
    for (index, (_, words)) in corpus.iter().enumerate().skip(start) {
        PROGRESS.store(index, std::sync::atomic::Ordering::Relaxed);
        println!("at {index}");
        vm.with_ctx(|c| {
            c.code = words.iter().copied().chain([a64::svc(0)]).collect();
            c.exceptions.clear();
            c.svc.clear();
            c.ticks_remaining = 1_000;
            c.ticks_used = 0;
        });
        // SAFETY: `jit` is live and not executing; the code behind it changed.
        unsafe { od_jit_clear_cache(jit) };
        for r in 0..31 {
            vm.set_reg(r, rng.next());
        }
        for v in 0..32 {
            vm.set_vec(v, [rng.next(), rng.next()]);
        }
        vm.set_sp(0x8_0000);
        vm.set_pc(CODE_BASE);
        for _ in 0..4 {
            let hr = vm.run();
            // SAFETY: `jit` is live and not executing.
            unsafe { od_jit_clear_halt(jit, hr) };
            if hr != OD_HALT_CACHE_INVALIDATION || vm.with_ctx(|c| c.ticks_remaining) == 0 {
                break;
            }
        }
    }
    println!("done");
}

/// Runs the sweep, restarting after every death; returns the words that took the process down.
fn sweep(test: &str, corpus: &[(String, Vec<u32>)], fastmem: bool) -> Vec<String> {
    let mut dead = Vec::new();
    let mut start = 0;
    loop {
        let exe = std::env::current_exe().expect("current_exe");
        let mut child = Command::new(exe)
            .args(["--exact", test, "--nocapture", "--test-threads", "1"])
            .env("OD_SWEEP_CHILD", start.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");
        let mut last = None;
        let mut finished = false;
        for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
            if let Some(i) = line.strip_prefix("at ") {
                last = i.parse::<usize>().ok();
            } else if line == "done" {
                finished = true;
            }
        }
        let status = child.wait().expect("wait");
        if finished && status.success() {
            return dead;
        }
        let Some(index) = last else {
            panic!("the sweep child died before executing anything: {status}");
        };
        let (name, words) = &corpus[index];
        dead.push(format!("{name} {words:08X?} ({status}, fastmem={fastmem})"));
        start = index + 1;
        assert!(dead.len() <= 64, "more than 64 deaths; stopping:\n{}", dead.join("\n"));
    }
}

fn child_start() -> Option<usize> {
    std::env::var("OD_SWEEP_CHILD").ok().and_then(|v| v.parse().ok())
}

#[test]
fn the_corpus_covers_the_whole_table() {
    let e = entries();
    // 643 active entries and 231 commented out on this pin (D5: 231 of 874 unimplemented).
    assert_eq!(e.len(), 874, "decoder entries parsed");
    assert_eq!(corpus().len(), 874 * variants());
}

#[test]
fn every_decoder_entry_executes_without_taking_the_process_down() {
    if let Some(start) = child_start() {
        run_from(&corpus(), start, true);
        return;
    }
    let dead = sweep("every_decoder_entry_executes_without_taking_the_process_down", &corpus(), true);
    assert!(dead.is_empty(), "{} word(s) took the process down:\n{}", dead.len(), dead.join("\n"));
}

#[test]
fn every_decoder_entry_executes_through_the_callback_path_too() {
    if let Some(start) = child_start() {
        run_from(&corpus(), start, false);
        return;
    }
    let dead = sweep("every_decoder_entry_executes_through_the_callback_path_too", &corpus(), false);
    assert!(dead.is_empty(), "{} word(s) took the process down:\n{}", dead.len(), dead.join("\n"));
}

#[test]
fn decoder_entries_in_pairs_behind_a_constant_execute_without_taking_the_process_down() {
    if let Some(start) = child_start() {
        run_from(&pair_corpus(), start, true);
        return;
    }
    let dead = sweep(
        "decoder_entries_in_pairs_behind_a_constant_execute_without_taking_the_process_down",
        &pair_corpus(),
        true,
    );
    assert!(dead.is_empty(), "{} program(s) took the process down:\n{}", dead.len(), dead.join("\n"));
}
