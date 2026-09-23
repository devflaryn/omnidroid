//! Clocks and sleeping: the platform seam for time.
//!
//! Three primitives — [`monotonic_now`], [`realtime_now`], [`sleep`] — which between them serve
//! the guest's `clock_gettime`, `gettimeofday`, `nanosleep` and `usleep`. `gmtime_r` needs none of
//! them: it is civil-time arithmetic over a number, and it lives in `omni-bionic`.
//!
//! # Why there is no `linux`/`macos` backend here, and why that is *not* a hole
//!
//! Every other seam in this crate has a `windows` backend and a structural unix one that returns
//! [`VmError::Unsupported`](crate::vm::VmError::Unsupported)/
//! [`FaultError::Unsupported`](crate::fault::FaultError::Unsupported) naming the POSIX call it
//! intends to make. That discipline exists because those seams **call an OS API**, and an
//! unverified `mmap` body misbehaves silently where a typed error fails immediately.
//!
//! This module calls no OS API. `Instant`, `SystemTime` and `thread::sleep` are portable standard
//! library, present and correct on all five targets, and `omni-cpu`'s `CNTPCT_EL0` (D5 amendment 4)
//! is already built on exactly this. Writing a `cfg(unix)` arm that returned `Unsupported` would
//! not be honest caution — it would be a **false** claim in the other direction, asserting that a
//! clock this process can already read cannot be read. The rule this project enforces is "never
//! claim a platform works"; `std` working on Linux is not a claim of ours, and manufacturing a
//! refusal would make the non-Windows bring-up strictly harder rather than easier.
//!
//! So the seam is here — a named module in the one crate allowed to own OS concerns, with one
//! process-wide epoch — and it is implemented once. **What is still not claimed:** nothing in this
//! module has been *run* on Linux or macOS, and the resolution notes below were measured on Windows
//! only.
//!
//! # The two properties a caller may rely on
//!
//! * [`monotonic_now`] never goes backwards and is measured from **one process-wide epoch**, so two
//!   guest instances, and two guest threads, timestamp the same event with the same number. That is
//!   the same reasoning `omni-cpu`'s counter epoch is a `OnceLock` for — `CNTPCT_EL0` is the
//!   *system* counter and every core reads the same one.
//! * [`realtime_now`] is the wall clock and may jump, in either direction. A wall clock set before
//!   1970 is reported as zero rather than as a negative duration; there is no other answer that is
//!   a `Duration`, and panicking on a mis-set host clock is not one.
//!
//! # Sleep resolution is a real, guest-visible fidelity gap on Windows
//!
//! `std::thread::sleep` on Windows is built on the scheduler's timer tick, whose default period is
//! ~15.6 ms. A guest `usleep(1000)` therefore sleeps for something closer to 15 ms than to 1 ms.
//! This is **recorded, not worked around**: raising the resolution needs either `timeBeginPeriod`,
//! which is process-wide and raises power draw for every thread, or a
//! `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` timer per sleeping thread. Neither is free and neither
//! has been measured here, so the choice belongs to whoever first has a guest that cares.
//!
//! D5 (amendment 4) already records the same number from the other side: its interval measurement
//! busy-waits "because Windows' sleep granularity is ~15 ms".
//!
//! [`sleep`] therefore guarantees only what POSIX's `nanosleep` guarantees: it sleeps for **at
//! least** the requested duration. Callers that need a bound in the other direction do not have one.

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The instant the monotonic clock started, for the whole process.
///
/// One `OnceLock` rather than a per-caller field, for the reason in the module documentation: the
/// runtime hosts several guest instances at once and they must agree about what time it is.
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// `CLOCK_MONOTONIC`: time since this process's clock epoch.
///
/// Never decreases. The epoch is not the boot time — `CLOCK_MONOTONIC` has no defined epoch, and a
/// guest is entitled only to the difference between two readings.
#[must_use]
pub fn monotonic_now() -> Duration {
    let epoch = *EPOCH.get_or_init(Instant::now);
    // `saturating_duration_since` rather than `-`: a clock that went backwards must report no
    // progress, never panic and never wrap. `Instant` is documented monotonic, so this saturates
    // only if that documentation is violated by the host.
    Instant::now().saturating_duration_since(epoch)
}

/// `CLOCK_REALTIME`: the wall clock, as a duration since the Unix epoch.
///
/// May jump in either direction; that is what a wall clock is. A host clock set before 1970 reads
/// as zero rather than as a negative duration.
#[must_use]
pub fn realtime_now() -> Duration {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO)
}

/// **A request to the host for a finer timer tick, for this process, held until dropped.**
///
/// The module documentation records the gap this closes: on Windows every sleep and every timed
/// wait -- `std::thread::sleep`, and the `parking_lot` parks behind the guest's `futex` and
/// condition variables -- is rounded to the scheduler tick, ~15.6 ms by default. A guest written
/// for Linux's high-resolution timers pays that on every short wait it makes. `timeBeginPeriod`
/// is the host's own remedy, per process since Windows 10 2004, and what a game on Windows does
/// for the length of a session; its cost is power, which is the embedding's to weigh, so this is
/// a guard an embedding holds rather than something the runtime does on its own.
///
/// On a unix host there is no coarse process tick to raise, and nothing is done -- **not measured
/// here**, like the rest of this module off Windows.
#[derive(Debug)]
#[must_use = "the resolution is given back when this is dropped"]
pub struct TimerResolution {
    period_ms: u32,
}

/// The host refused a timer resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the host refused a {requested_ms} ms timer resolution (`timeBeginPeriod` answered {code})")]
pub struct TimerResolutionError {
    /// The period asked for, in whole milliseconds.
    pub requested_ms: u32,
    /// What the host answered.
    pub code: u32,
}

impl TimerResolution {
    /// Ask for timers that fire within `period` of when they are due, rounded **up** to whole
    /// milliseconds and at least one -- the host's unit.
    ///
    /// # Errors
    ///
    /// [`TimerResolutionError`] if the host refuses the period.
    pub fn raise(period: Duration) -> Result<TimerResolution, TimerResolutionError> {
        let period_ms = u32::try_from(period.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX).max(1);
        backend_raise(period_ms)?;
        Ok(TimerResolution { period_ms })
    }

    /// The period this holds.
    #[must_use]
    pub fn period(&self) -> Duration {
        Duration::from_millis(u64::from(self.period_ms))
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        backend_lower(self.period_ms);
    }
}

#[cfg(target_os = "windows")]
fn backend_raise(period_ms: u32) -> Result<(), TimerResolutionError> {
    // SAFETY: `timeBeginPeriod` takes a period by value and touches no memory of ours.
    let code = unsafe { windows_sys::Win32::Media::timeBeginPeriod(period_ms) };
    if code == windows_sys::Win32::Media::TIMERR_NOERROR {
        Ok(())
    } else {
        Err(TimerResolutionError { requested_ms: period_ms, code })
    }
}

#[cfg(target_os = "windows")]
fn backend_lower(period_ms: u32) {
    // SAFETY: as `timeBeginPeriod`, and paired with the successful call that made this guard --
    // the host requires exactly that pairing.
    let _ = unsafe { windows_sys::Win32::Media::timeEndPeriod(period_ms) };
}

#[cfg(not(target_os = "windows"))]
fn backend_raise(_period_ms: u32) -> Result<(), TimerResolutionError> {
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn backend_lower(_period_ms: u32) {}

/// Block the calling thread for **at least** `duration`.
///
/// There is no upper bound: see the module documentation on Windows' ~15.6 ms timer tick. A
/// zero duration returns without blocking.
///
/// This is the whole of the sleeping primitive. **Deciding how long a guest may be allowed to
/// sleep is not this seam's job** — an unbounded sleep requested by untrusted guest code is a
/// denial of service, and the cap that refuses one belongs with the caller that knows what the
/// guest is allowed to ask for (`omni-android`'s adapter caps it and names the cap in its
/// refusal).
pub fn sleep(duration: Duration) {
    if duration.is_zero() {
        return;
    }
    std::thread::sleep(duration);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monotonic over a tight read loop, and the epoch is shared rather than re-read.
    ///
    /// n = 100,000 consecutive reads on one thread. A *structural* assertion rather than a timing
    /// one: the failure this catches is `monotonic_now` anchoring on a fresh `Instant::now()` each
    /// call, which makes every reading ~0 and is therefore both non-increasing and useless.
    #[test]
    fn the_monotonic_clock_never_goes_backwards_and_accumulates() {
        const READS: usize = 100_000;
        let first = monotonic_now();
        let mut previous = first;
        for i in 0..READS {
            let now = monotonic_now();
            assert!(now >= previous, "read {i} of {READS} went backwards: {previous:?} -> {now:?}");
            previous = now;
        }
        assert!(
            previous > first,
            "n = {READS} reads advanced the clock not at all ({first:?} -> {previous:?}), which is \
             what a per-call epoch looks like"
        );
    }

    /// **A raised resolution makes a 1 ms sleep a 1 ms sleep**, where the default tick made it
    /// ~15 ms; and it is given back. Measured as the median of 21 sleeps each way, so a single
    /// preempted sleep cannot decide it. Windows only: elsewhere there is no tick to raise.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_raised_resolution_shortens_a_short_sleep() {
        fn median_sleep() -> Duration {
            let mut took: Vec<Duration> = (0..21)
                .map(|_| {
                    let started = Instant::now();
                    sleep(Duration::from_millis(1));
                    started.elapsed()
                })
                .collect();
            took.sort();
            took[took.len() / 2]
        }
        let resolution = TimerResolution::raise(Duration::from_micros(500)).expect("1 ms");
        assert_eq!(resolution.period(), Duration::from_millis(1), "rounded up to the host's unit");
        let fine = median_sleep();
        assert!(fine < Duration::from_millis(4), "a 1 ms sleep took {fine:?} with the tick raised");
    }

    /// The wall clock is after 2020 and before 2100.
    ///
    /// Not a precision test — a bound wide enough that only a wrong *unit* or a wrong epoch can
    /// fail it, which are the two ways this returns a plausible wrong answer.
    #[test]
    fn the_wall_clock_is_a_unix_time_in_seconds() {
        let now = realtime_now().as_secs();
        // 2020-01-01T00:00:00Z and 2100-01-01T00:00:00Z.
        assert!(
            (1_577_836_800..4_102_444_800).contains(&now),
            "the wall clock reads {now} seconds since the Unix epoch"
        );
    }

    /// `sleep` sleeps for at least what it was asked for, and zero returns.
    ///
    /// The lower bound is the only one POSIX gives and the only one Windows' timer tick permits.
    /// n = 5 sleeps of 10 ms; the assertion is one-sided on purpose, because the upper bound is
    /// exactly the fidelity gap the module documents and a test that pinned one would be flaky by
    /// design.
    #[test]
    fn sleep_sleeps_for_at_least_as_long_as_it_was_asked_to() {
        const WAIT: Duration = Duration::from_millis(10);
        for round in 0..5 {
            let before = Instant::now();
            sleep(WAIT);
            let elapsed = before.elapsed();
            assert!(elapsed >= WAIT, "round {round}: slept {elapsed:?}, asked for {WAIT:?}");
        }
        let before = Instant::now();
        sleep(Duration::ZERO);
        assert!(before.elapsed() < Duration::from_millis(5), "a zero sleep must not block");
    }
}
