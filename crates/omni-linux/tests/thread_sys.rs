//! `[thread-sys]` (with `OMNI_THREAD_CPU`), end to end: a real NDK program (`fixtures/syswait`)
//! whose thread `sys-mixed` alternates 2 ms of work with a 2 ms condition-variable wait in
//! `wait_here`. Its report must put about half the thread's wall time in `futex(wait)` -- with
//! little processor time in it -- and name `wait_here` as the code that waited, through libc's
//! `pthread_cond_timedwait` (the return addresses at the call, the frame-pointer chain).
//!
//! Its own test binary: `OMNI_THREAD_CPU` is read once per host process.
#![cfg(target_arch = "x86_64")]
mod common;

use omni_linux::ExitStatus;

/// `name 123 ms/s` in a `[thread-sys]` row: the ms/s.
fn ms_per_s(line: &str, name: &str) -> Option<f64> {
    let at = line.find(&format!("{name} "))? + name.len() + 1;
    line[at..].split(' ').next()?.parse().ok()
}

#[test]
fn a_hot_thread_s_futex_waits_are_timed_and_named_after_the_code_that_waits() {
    std::env::set_var("OMNI_THREAD_CPU", "1");
    let Some((status, out, err)) = common::run_fixture("syswait", &["3.5"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stdout: {out}\nstderr: {err}");
    assert_eq!(out, "syswait ok 1\n");

    let reports = omni_linux::threadsys::recent_reports();
    assert!(!reports.is_empty(), "no [thread-sys] report in 3.5 s of a busy thread");
    let mut whole_periods = 0;
    for r in &reports {
        eprint!("{r}");
        let lines: Vec<&str> = r.lines().collect();
        let Some(at) = lines.iter().position(|l| l.contains("\"sys-mixed\"")) else { continue };
        let head = lines[at];
        let Some(wait) = ms_per_s(head, "futex(wait)") else { continue };
        // A period the thread spent wholly in its loop (the first and last are partial).
        if !(300.0..=750.0).contains(&wait) {
            continue;
        }
        whole_periods += 1;
        let cpu: f64 = head
            .split("futex(wait) ")
            .nth(1)
            .and_then(|s| s.split("(cpu ").nth(1))
            .and_then(|s| s.split(')').next())
            .and_then(|s| s.parse().ok())
            .expect("futex(wait)'s processor time");
        assert!(cpu < wait / 4.0, "a wait is mostly blocked, not running: {head}");
        let waiter = lines.get(at + 1).copied().unwrap_or_default();
        assert!(waiter.contains("futex wait "), "the thread's top waiting stack follows it: {r}");
        assert!(waiter.contains(": syswait+0x4ba0 wait_here"), "the code that waited is wait_here: {waiter}");
        assert!(waiter.contains("(via libc.so"), "through libc: {waiter}");
    }
    assert!(whole_periods >= 1, "no whole period of sys-mixed with ~half its time in futex(wait)");
}
