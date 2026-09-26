//! Milestone A5: signals delivered as the arm64 kernel delivers them -- a fault to a handler on
//! the alternate stack that returns and retries, raise, blocking and unblocking, pthread_kill to a
//! waiting thread, and registers kept across a handler.
mod common;

use common::run_fixture;
use omni_linux::ExitStatus;

#[test]
fn a5_signals() {
    let Some((status, out, err)) = run_fixture("signals", &[]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stdout: {out}\nstderr: {err}");
    assert_eq!(out, "signals ok\n");
}

#[test]
fn a_vdso_page_holds_the_kernels_sigreturn_trampoline() {
    let Some((status, out, err)) = common::run(&["/system/bin/toybox", "cat", "/proc/self/maps"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    assert!(out.lines().any(|l| l.ends_with("[vdso]") && l.contains(" r-xp ")), "{out}");
}
