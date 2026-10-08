//! `OMNI_GUEST_PROF=1` names the guest function a hot thread runs, end to end: a real NDK program
//! (`fixtures/hotloop`) spends its time in `spin_hot` on a thread named `hot-worker`; the sampler
//! (`OMNI_THREAD_CPU=1`) takes its host instruction pointer, the shared code cache gives the guest
//! block (patch 0036), the process's memory map the file, and the file's `.symtab` the symbol.
//!
//! Its own test binary: both switches are read once per host process.
#![cfg(target_arch = "x86_64")]
mod common;

use omni_linux::ExitStatus;

#[test]
fn the_hot_function_of_a_named_thread_heads_its_guestprof_report() {
    std::env::set_var("OMNI_THREAD_CPU", "1");
    std::env::set_var("OMNI_GUEST_PROF", "1");
    let Some((status, out, err)) = common::run_fixture("hotloop", &["3.5"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stdout: {out}\nstderr: {err}");
    assert_eq!(out, "hotloop ok 1\n");

    let reports = omni_linux::guestprof::recent_reports();
    assert!(!reports.is_empty(), "no [guestprof] report in 3.5 s of a hot thread");
    let mut best = 0.0f64;
    for r in &reports {
        eprint!("{r}");
        let lines: Vec<&str> = r.lines().collect();
        let Some(at) = lines.iter().position(|l| l.contains("\"hot-worker\"")) else { continue };
        // The thread's first row is its hottest function.
        let top = lines.get(at + 1).copied().unwrap_or_default();
        if !(top.contains(" hotloop+0x") && top.contains(" spin_hot ")) {
            continue;
        }
        let pct: f64 = top.rsplit(' ').next().and_then(|p| p.trim_end_matches('%').parse().ok()).unwrap_or(0.0);
        best = best.max(pct);
    }
    assert!(best >= 50.0, "spin_hot heads hot-worker's report with {best}% of its samples");
}
