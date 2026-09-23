//! The clock seam on Linux: **is a 1 ms sleep a 1 ms sleep here?**
//!
//! ```text
//! cargo test -p omni-platform --release --test clock_linux -- --nocapture
//! ```
//!
//! `clock.rs` is shared, and its non-Windows [`TimerResolution`] arm returns `Ok` and does
//! nothing, with the comment "on a unix host there is no coarse process tick to raise ... **not
//! measured here**". That `Ok` is a claim -- that nothing needed raising -- and this file is what
//! makes it a measured one on this host rather than an assumption carried over from the absence
//! of `timeBeginPeriod`.
//!
//! **Method.** `clock::sleep(1 ms)` timed with `Instant` (the same `CLOCK_MONOTONIC` the seam's
//! own `monotonic_now` reads), n = 41 per condition, **median** reported and asserted -- a single
//! preempted sleep cannot decide it (the Windows test's own method, VERIFICATION entry 9). The
//! upper bound, 2 ms, is set against the failure this would catch: a tick-grained sleep rounds
//! up to the tick, 4 ms at this kernel's `CONFIG_HZ=250` and 15.6 ms on Windows. Linux's default
//! timer slack for a normal thread is 50 us, which is the expected overshoot.
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use omni_platform::clock::{sleep, TimerResolution};

const N: usize = 41;

/// n sleeps of `requested`, sorted.
fn sleeps(requested: Duration) -> Vec<Duration> {
    let mut took: Vec<Duration> = (0..N)
        .map(|_| {
            let started = Instant::now();
            sleep(requested);
            started.elapsed()
        })
        .collect();
    took.sort();
    took
}

fn summary(label: &str, took: &[Duration]) -> Duration {
    let median = took[took.len() / 2];
    eprintln!(
        "{label}: n = {}, min {:?}, median {median:?}, p90 {:?}, max {:?}",
        took.len(),
        took[0],
        took[took.len() * 9 / 10],
        took[took.len() - 1]
    );
    median
}

/// **A 1 ms sleep takes about 1 ms here, with no resolution raised and with one held**, so the
/// non-Windows arm's `Ok` states something true about this host: there was no coarse tick to
/// raise, and raising "it" changes nothing.
///
/// Both halves are asserted, and the second is the one a mutation of `clock.rs` could reach: a
/// non-Windows arm that refused would fail `raise`, and one that did something to the process
/// that made sleeps coarser would fail the median.
#[test]
fn a_one_millisecond_sleep_is_about_one_millisecond_with_or_without_a_raised_resolution() {
    let one = Duration::from_millis(1);
    let default = sleeps(one);
    let default_median = summary("1 ms sleep, nothing raised", &default);
    assert!(default[0] >= one, "a sleep returned early: {:?}", default[0]);
    assert!(
        default_median < Duration::from_millis(2),
        "the median 1 ms sleep took {default_median:?}: this host rounds sleeps to a tick"
    );

    let held = TimerResolution::raise(Duration::from_micros(500)).expect("Ok on a unix host");
    assert_eq!(held.period(), one, "rounded up to whole milliseconds, as on Windows");
    let raised = sleeps(one);
    let raised_median = summary("1 ms sleep, resolution held", &raised);
    drop(held);
    assert!(raised[0] >= one);
    assert!(raised_median < Duration::from_millis(2), "{raised_median:?}");
}

/// The same measurement at 100 us, which says how fine the grain actually is: a sleep this short
/// is dominated by the timer slack and the wakeup path, and it is recorded so that a guest's
/// `usleep(100)` has a number beside it.
#[test]
fn a_hundred_microsecond_sleep_is_recorded_with_its_overshoot() {
    let took = sleeps(Duration::from_micros(100));
    let median = summary("100 us sleep", &took);
    assert!(took[0] >= Duration::from_micros(100));
    assert!(median < Duration::from_millis(1), "a 100 us sleep's median was {median:?}");
}
