//! A sampled host instruction pointer inside a shared code cache names the guest code it runs
//! (vendored patch 0036, `stats::guest_pcs_of`) -- what `omni-linux`'s `OMNI_GUEST_PROF` reports.
//!
//! A guest thread spins in a known function (`hot`) after passing through another (`entry`); this
//! thread samples its host instruction pointer as `omni-linux`'s `cpuprof` does, keeps the samples
//! that are in a code cache, and asks which guest block each is in. Nearly all must be `hot`'s:
//! the loop is a single block that links to itself, so only the run loop's slice boundaries (the
//! dispatcher, the prelude: `u64::MAX`) take any time elsewhere.
#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use std::sync::mpsc;
use std::time::Duration;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::{ExitReason, GuestCpu, RunLimit};
use omni_platform::sampler::{self, HostThread, MemoryKind, CODE_AFTER, CODE_BEFORE};

/// Where `hot` starts in the code region, in bytes: away from `entry`, so a block of one cannot be
/// taken for the other.
const HOT_AT: usize = 0x400;

#[test]
fn a_sampled_host_address_in_the_code_cache_names_the_guest_block_that_runs_there() {
    if !harness::shared_code_cache_asked() {
        eprintln!("no shared code cache (OMNI_JIT_SHARED_CACHE=0): nothing to resolve against");
        return;
    }
    let guest = Guest::new();
    // entry: x1 = 0, then into hot. hot: x1 += 1, x2 += 3, forever.
    let entry = guest.load(&[movz(1, 0, 0), movz(2, 0, 0), b(((HOT_AT - 8) / 4) as i32)]);
    let hot = guest.load_at(HOT_AT, &[add_imm(1, 1, 1), add_imm(2, 2, 3), b(-2)]);
    let (mut cpu, _) = guest.thread();
    let halt = cpu.halt_handle();

    let (tx, rx) = mpsc::channel();
    let runner = std::thread::spawn(move || {
        tx.send(HostThread::current().expect("this thread, for sampling")).expect("the sampler waits");
        let exit = cpu.run(entry, RunLimit::Unlimited).expect("the loop runs");
        (exit, cpu.x(x(1)))
    });
    let host = rx.recv().expect("the guest thread's handle");

    let mut code = [0u8; CODE_BEFORE + CODE_AFTER];
    let mut jit = Vec::new();
    let mut taken = 0;
    std::thread::sleep(Duration::from_millis(50)); // past translation
    while taken < 300 {
        std::thread::sleep(Duration::from_millis(2));
        let Ok(s) = host.sample(&mut code) else { continue };
        taken += 1;
        if matches!(sampler::memory_kind(s.ip), Ok(MemoryKind::PrivateWritableExecutable { .. })) {
            jit.push(s.ip as u64);
        }
    }
    halt.request();
    let (exit, x1) = runner.join().expect("the guest thread");
    assert!(matches!(exit, ExitReason::Halted { .. }), "{exit}");
    assert!(x1 > 1_000_000, "the loop ran ({x1} iterations)");

    assert!(jit.len() >= 200, "{} of {taken} samples in translated code", jit.len());
    jit.sort_unstable();
    let pcs = omni_cpu::stats::guest_pcs_of(&jit);
    let in_hot = pcs.iter().filter(|&&pc| pc == hot as u64).count();
    let elsewhere: Vec<u64> = pcs.iter().copied().filter(|&pc| pc != hot as u64 && pc != u64::MAX).collect();
    eprintln!(
        "{} jit samples: {in_hot} in hot ({hot:#x}), {} unresolved, {} elsewhere {elsewhere:x?}",
        jit.len(),
        pcs.iter().filter(|&&pc| pc == u64::MAX).count(),
        elsewhere.len()
    );
    assert!(elsewhere.is_empty(), "no sample is in a block the thread is not running: {elsewhere:x?}");
    assert!(in_hot * 10 >= jit.len() * 9, "{in_hot} of {} samples resolved to hot", jit.len());
}
