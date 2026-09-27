//! Milestone A4: real bionic threads -- `pthread_create`/`join`, mutexes, condition variables,
//! `__thread` -- in an NDK-built program, and `exit` from a secondary thread ending the process.
mod common;

use std::time::{Duration, Instant};

use common::run_fixture;
use omni_linux::ExitStatus;

#[test]
fn a4_threads_mutexes_condvars_and_tls() {
    let Some((status, out, err)) = run_fixture("threads", &[]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stdout: {out}\nstderr: {err}");
    assert_eq!(out, "threads ok 800000\n");
}

#[test]
fn a4_exit_from_a_thread_ends_the_process_however_main_is_blocked() {
    let start = Instant::now();
    let Some((status, out, err)) = run_fixture("thread_exit", &[]) else { return };
    assert_eq!(status, ExitStatus::Exited(3), "stdout: {out}\nstderr: {err}");
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}

/// Priority-inheritance mutexes (`FUTEX_LOCK_PI`, `UNLOCK_PI`, `TRYLOCK_PI`): audioserver's
/// audio_utils mutexes are these, and `std::mutex` aborted on `ENOSYS`.
#[test]
fn priority_inheritance_mutexes_lock_contend_and_time_out() {
    let Some((status, out, err)) = common::run_fixture("pimutex", &[]) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 9, "{out}\n{err}");
    assert_eq!(status, omni_linux::ExitStatus::Exited(0), "{out}\n{err}");
}
