//! `OMNI_PROC_CPU`, end to end: while a real NDK program (`fixtures/syswait`) runs, its thread
//! `sys-mixed` (2 ms of work, 2 ms of waiting) is counted among the process's *guest* threads by its
//! task name, at about half a core, next to the host threads; and as the hottest thread it is sampled.
//!
//! Its own test binary: `OMNI_PROC_CPU` is read once per host process.
#![cfg(windows)]
mod common;

use std::time::Duration;

use omni_linux::proccpu::Profiler;

#[test]
fn a_guest_task_is_counted_by_its_name_among_every_thread_of_the_process() {
    std::env::set_var("OMNI_PROC_CPU", "1");
    if common::sysroot().is_none() {
        eprintln!("SKIPPED: no sysroot");
        return;
    }
    let run = std::thread::spawn(|| common::run_fixture("syswait", &["5"]));
    std::thread::sleep(Duration::from_millis(1000));
    let mut p = Profiler::default();
    assert!(p.period().is_none(), "the baseline");
    let mut reports = Vec::new();
    for _ in 0..3 {
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(10));
            p.tick();
        }
        reports.push(p.period().expect("a report"));
    }
    let (status, out, _) = run.join().expect("the program's thread").expect("a sysroot");
    assert_eq!(status, omni_linux::ExitStatus::Exited(0), "{out}");
    for r in &reports {
        eprint!("{r}");
    }
    // The program's start (loading, the main thread busy) may take the first period; a period it
    // spent wholly in its loop shows sys-mixed at about half a core.
    let shares: Vec<f64> = reports
        .iter()
        .filter_map(|r| {
            let guest = r.lines().find(|l| l.contains("  guest: "))?;
            guest.split("sys-mixed ").nth(1)?.split('%').next()?.parse().ok()
        })
        .collect();
    assert!(shares.iter().any(|p| (25.0..=75.0).contains(p)), "sys-mixed at about half a core in a period: {shares:?}");
    // A period samples the previous one's hottest threads: sys-mixed among them, as a guest task.
    assert!(reports[1..].iter().any(|r| r.contains("\"sys-mixed\" (guest)")), "sys-mixed sampled");
}
