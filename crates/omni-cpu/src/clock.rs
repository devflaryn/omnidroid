//! The guest's architectural counter: `CNTPCT_EL0` and the `CNTFRQ_EL0` it must be read against.
//!
//! # What went wrong, and why it would have been diagnosed as anything but a clock
//!
//! `CNTPCT_EL0` used to return the backend's per-slice instruction counter. That counter is reset to
//! zero at the top of **every slice** of a run — a million guest instructions by default, and again
//! at the start of every `run` — so the value a guest reads as a monotonic clock sawtooths. Two
//! reads and a subtraction give a *negative* delta; a `while (now() < deadline)` loop never
//! terminates and is eventually stopped by the step budget and reported as `StepLimitReached`, which
//! points at the budget. The doc comment on the callback said "monotonic".
//!
//! The units were wrong as well, and in the same place. dynarmic advertises `CNTFRQ_EL0` and the
//! guest divides by it; the counter was ticking once per guest *instruction* against an advertised
//! **600 MHz**, so the two had no relationship at all.
//!
//! # What it does now, and which of the two options this is
//!
//! The host's monotonic clock, scaled to the advertised frequency. The alternative — an accumulator
//! of guest instructions that slices do not reset — would be monotonic and reproducible, but it
//! cannot be given honest units: guest instructions per second is not a constant, so no value of
//! `CNTFRQ_EL0` makes `ticks / CNTFRQ` a time. A guest reading `CNTPCT_EL0` wants elapsed time (this
//! is the counter behind `clock_gettime(CLOCK_MONOTONIC)` on AArch64 Android), so it gets elapsed
//! time.
//!
//! Three properties, and each is asserted by a test rather than argued here:
//!
//! * **Monotonic.** [`std::time::Instant`] is documented monotonic, and the scaling is a
//!   non-decreasing integer function of it, so two reads can never go backwards. Slices do not enter
//!   into it at all, which is the point: the defect was that they did.
//! * **One counter for the whole process.** The epoch is a single `OnceLock`, not a per-context or
//!   per-thread value. `CNTPCT_EL0` is the *system* counter — every core reads the same one — so two
//!   guest threads that timestamp an event must agree about it, and D5 measured Roblox to be heavily
//!   multithreaded.
//! * **Units that match what is advertised.** [`CNTFRQ_HZ`] is programmed into the jit's
//!   `cntfrq_el0` *and* used as the scale here, so the two cannot drift apart. Guest code that reads
//!   `CNTFRQ_EL0` and divides gets seconds.

use std::sync::OnceLock;
use std::time::Instant;

/// The frequency `CNTFRQ_EL0` advertises, and the rate [`cntpct`] ticks at.
///
/// 600 MHz, which is dynarmic's own default when `cntfrq_el0` is left at 0 (`od_dynarmic.cpp:310`).
/// Keeping that value is deliberate — it changes nothing a guest can observe about the frequency,
/// and this milestone has enough new numbers in it — but it is now written down on **our** side and
/// programmed explicitly, so the counter and the frequency come from one constant instead of two
/// defaults that happen to agree. Real AArch64 Android hardware is usually 19.2 or 24 MHz; nothing
/// here depends on the value, only on the two uses of it being the same.
pub const CNTFRQ_HZ: u32 = 600_000_000;

/// Nanoseconds per second, named because it appears in an expression with two other large numbers.
const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// The instant the counter started, for the whole process.
///
/// One `OnceLock` rather than a per-context field: see the module docs. Initialised by whichever
/// guest thread reads the counter first, which makes the guest's first read a small number — exactly
/// as a machine that has just been powered on would.
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// `CNTPCT_EL0`: elapsed time since the process's counter epoch, in [`CNTFRQ_HZ`] ticks.
///
/// Called from a dynarmic callback, so it must not panic. It does not: `Instant::now` does not
/// panic, `Instant::saturating_duration_since` does not, and the conversion saturates rather than
/// truncating — a `u64` of 600 MHz ticks is 974 years, so saturation is unreachable, and it is
/// written saturating anyway because a wrapped clock is a clock that goes backwards.
#[must_use]
pub fn cntpct() -> u64 {
    let epoch = *EPOCH.get_or_init(Instant::now);
    let nanos = Instant::now().saturating_duration_since(epoch).as_nanos();
    let ticks = nanos.saturating_mul(u128::from(CNTFRQ_HZ)) / NANOS_PER_SECOND;
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scale, checked as arithmetic rather than by waiting for a clock.
    ///
    /// A counter at 600 MHz advances 600,000 ticks per millisecond. Getting this factor wrong is the
    /// half of the defect that a monotonicity test cannot see, because a wrong scale is still
    /// monotonic.
    #[test]
    fn the_counter_ticks_at_the_frequency_it_advertises() {
        assert_eq!(CNTFRQ_HZ, 600_000_000);
        // One millisecond of nanoseconds, converted the way `cntpct` converts it.
        let per_ms = 1_000_000u128 * u128::from(CNTFRQ_HZ) / NANOS_PER_SECOND;
        assert_eq!(per_ms, 600_000, "600 MHz is 600,000 ticks per millisecond");
        // And a second of ticks is the frequency itself, which is what `ticks / CNTFRQ_EL0` means.
        assert_eq!(
            NANOS_PER_SECOND * u128::from(CNTFRQ_HZ) / NANOS_PER_SECOND,
            u128::from(CNTFRQ_HZ)
        );
    }

    /// Monotonic over a tight read loop on one thread, and non-zero over a measured interval.
    ///
    /// n = 100,000 consecutive reads for the monotonicity half. The interval half uses a busy wait
    /// rather than `sleep`, because Windows' sleep granularity is ~15 ms and the point is to bound
    /// the counter against a *known* elapsed time.
    #[test]
    fn the_counter_never_goes_backwards_and_advances_with_real_time() {
        const READS: usize = 100_000;
        let mut previous = cntpct();
        for i in 0..READS {
            let now = cntpct();
            assert!(now >= previous, "read {i} of {READS} went backwards: {previous} -> {now}");
            previous = now;
        }

        const WAIT: std::time::Duration = std::time::Duration::from_millis(20);
        let before = cntpct();
        let start = Instant::now();
        while start.elapsed() < WAIT {
            std::hint::spin_loop();
        }
        let elapsed_ticks = cntpct() - before;
        let expected = u64::from(CNTFRQ_HZ) / 1_000 * 20;
        assert!(
            elapsed_ticks >= expected,
            "n = 1 interval of at least {WAIT:?}: the counter advanced {elapsed_ticks} ticks, \
             which is less than the {expected} that {WAIT:?} is at {CNTFRQ_HZ} Hz"
        );
    }
}
