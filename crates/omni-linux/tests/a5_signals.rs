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

/// A2-A5 review, Important 5: an undefined instruction and `brk` reach their handlers.
#[test]
fn an_undefined_instruction_raises_sigill_and_brk_raises_sigtrap() {
    for (mode, want) in [("ill", "ill ok\n"), ("trap", "trap ok\n")] {
        let Some((status, out, err)) = run_fixture("faults", &[mode]) else { return };
        assert_eq!(status, ExitStatus::Exited(0), "{mode}: stdout: {out}\nstderr: {err}");
        assert_eq!(out, want);
    }
}

/// A2-A5 review, Important 4: a fault the task blocks or ignores is fatal, as the kernel's
/// `force_sig` makes it.
#[test]
fn a_fault_while_sigsegv_is_blocked_or_ignored_kills() {
    for mode in ["blocked", "ignored"] {
        let Some((status, out, err)) = run_fixture("faults", &[mode]) else { return };
        assert!(matches!(status, ExitStatus::Killed { signal: 11, .. }), "{mode}: {status:?}\nstdout: {out}\nstderr: {err}");
        assert_eq!(out, "faulting\n");
    }
}
