//! POSIX timers (`crate::timer`) from a guest through the real bionic (`tests/fixtures/timers.c`):
//! a `SIGEV_THREAD` timer's callback runs periodically on bionic's own thread (woken by `SI_TIMER`
//! in `rt_sigtimedwait`) and stops at `timer_delete`; a `SIGEV_SIGNAL` handler gets `SI_TIMER` and
//! the timer's value; expiries while the signal is blocked are counted as overruns (`si_overrun`,
//! `timer_getoverrun`); `SIGEV_THREAD_ID` reaches the thread it names.
mod common;

#[test]
fn posix_timers_fire_as_bionic_expects() {
    let Some((status, out, err)) = common::run_fixture("timers", &[]) else { return };
    eprintln!("{out}");
    assert!(matches!(status, omni_linux::ExitStatus::Exited(0)), "{status:?}\n{out}\n{err}");
    assert!(out.contains("\nok\n") || out.ends_with("ok\n"), "{out}");
}
