//! `gmtime_r`: a Unix timestamp as broken-down UTC.
//!
//! # Why this is in the pure crate and not in the adapter
//!
//! It needs no clock. `gmtime_r` is handed a `time_t` and converts it; *reading* the clock is
//! `time` or `clock_gettime`, and those are somebody else's. And `gmtime` is **UTC by definition**,
//! so unlike `localtime_r` it needs no timezone database, no `TZ` variable and no host locale. What
//! is left is integer arithmetic over a proleptic Gregorian calendar, which is exactly what this
//! crate is for — and it means the conversion can be tested and mutated without a guest, a boundary
//! or a CPU.
//!
//! # The calendar algorithm, and why this one
//!
//! `civil_from_days` from Howard Hinnant's *chrono-Compatible Low-Level Date Algorithms*, the same
//! derivation C++20's `<chrono>` is specified against. It is used here rather than a table walk for
//! one reason that matters to this project: it is **branch-free over the whole i64 range** and has
//! no accumulating loop, so a hostile `time_t` cannot make it slow. A "step a year at a time from
//! 1970" implementation is the obvious alternative and it turns `gmtime_r(INT64_MAX)` into a
//! hundred-billion-iteration loop inside a thunk handler — a denial of service reachable from one
//! guest argument.
//!
//! The era arithmetic works because the Gregorian calendar repeats exactly every 400 years
//! (146,097 days), and the algorithm shifts the year to start in March so that the leap day is the
//! last day of the year and needs no special case.
//!
//! # What is refused rather than wrapped
//!
//! `struct tm`'s fields are `int`. A `time_t` far enough from 1970 produces a year that does not
//! fit one, and C's answer to that is [`GmtimeError::YearOutOfRange`] — glibc returns `NULL` and
//! sets `EOVERFLOW`, and that is what the adapter does with this. Wrapping the year instead would
//! produce a **date**, which is the worst available outcome: a plausible answer, thousands of years
//! wrong, from a function whose result gets printed into a log.
//!
//! # Not verified against an NDK header
//!
//! [`TM_BYTES`] and the offsets below are derived from bionic's `<time.h>` field by field, like
//! [`crate::layouts`]'s constants and for the same reason: there is no NDK on the machine this was
//! written on. The derivation is in the [`TM_BYTES`] documentation. Unlike the `pthread_*` sizes,
//! this one has an independent check available to whoever gets an NDK — every field is an `int`
//! except the last two, so the layout is forced by the C rules once the field *order* is right, and
//! the field order is published in POSIX plus two BSD extensions bionic inherits.

use crate::memory::{checked_range, Fault, GuestMemory};

/// Bytes of bionic's LP64 `struct tm`.
///
/// **Derived from bionic's `<time.h>`, field by field, and not verified against an NDK.**
///
/// | offset | bytes | field |
/// |---|---|---|
/// | 0 | 4 | `int tm_sec` |
/// | 4 | 4 | `int tm_min` |
/// | 8 | 4 | `int tm_hour` |
/// | 12 | 4 | `int tm_mday` |
/// | 16 | 4 | `int tm_mon` |
/// | 20 | 4 | `int tm_year` |
/// | 24 | 4 | `int tm_wday` |
/// | 28 | 4 | `int tm_yday` |
/// | 32 | 4 | `int tm_isdst` |
/// | 36 | 4 | padding, so the `long` that follows is 8-aligned |
/// | 40 | 8 | `long tm_gmtoff` |
/// | 48 | 8 | `const char *tm_zone` |
/// | **56** | | end |
///
/// The first nine fields and their order are POSIX. `tm_gmtoff` and `tm_zone` are the BSD
/// extensions glibc and bionic both carry, in that order, at the end. On LP64 `long` is 8 bytes and
/// so is a pointer, which forces the 4 bytes of padding at offset 36.
pub const TM_BYTES: usize = 56;

/// Offset of `tm_gmtoff` inside a guest `struct tm`.
pub const TM_GMTOFF_OFFSET: usize = 40;

/// Offset of `tm_zone` inside a guest `struct tm`.
pub const TM_ZONE_OFFSET: usize = 48;

/// Seconds in a day.
const SECONDS_PER_DAY: i64 = 86_400;

/// Days in one 400-year Gregorian era.
const DAYS_PER_ERA: i64 = 146_097;

/// Days from 0000-03-01 (the era origin) to 1970-01-01, which is what shifts a Unix day count
/// onto the era arithmetic.
const DAYS_ERA_TO_UNIX_EPOCH: i64 = 719_468;

/// 1970-01-01 was a **Thursday**, and `tm_wday` counts Sunday as 0.
const UNIX_EPOCH_WEEKDAY: i64 = 4;

/// Broken-down UTC time: the nine POSIX fields of `struct tm`.
///
/// `i32` rather than `i64` because the guest's fields are `int`, and the conversion that can fail
/// is the one this crate must not paper over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tm {
    /// `tm_sec`: 0-59. **Never 60**: UTC as computed from a `time_t` has no leap seconds in it,
    /// because a `time_t` does not count them.
    pub sec: i32,
    /// `tm_min`: 0-59.
    pub min: i32,
    /// `tm_hour`: 0-23.
    pub hour: i32,
    /// `tm_mday`: 1-31.
    pub mday: i32,
    /// `tm_mon`: **0-11**, January is 0.
    pub mon: i32,
    /// `tm_year`: years **since 1900**, so 2026 is 126.
    pub year: i32,
    /// `tm_wday`: 0-6, Sunday is 0.
    pub wday: i32,
    /// `tm_yday`: 0-365, January 1st is 0.
    pub yday: i32,
    /// `tm_isdst`: always 0 for UTC. Not "unknown" (-1): UTC provably has no daylight saving,
    /// and -1 would tell `mktime` to go and work it out.
    pub isdst: i32,
}

/// Why a `time_t` could not be broken down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GmtimeError {
    /// The year does not fit `struct tm`'s `int tm_year`.
    ///
    /// C's answer is `NULL` with `EOVERFLOW`. The alternative — wrapping — produces a date that
    /// looks entirely reasonable and is wrong by billions of years.
    YearOutOfRange {
        /// The proleptic Gregorian year the timestamp falls in, before the `- 1900` that would
        /// have overflowed.
        year: i64,
    },
}

impl core::fmt::Display for GmtimeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            GmtimeError::YearOutOfRange { year } => write!(
                f,
                "the timestamp falls in the year {year}, and `struct tm`'s `int tm_year` holds \
                 years since 1900 — the value does not fit an int, which C reports as EOVERFLOW"
            ),
        }
    }
}

impl std::error::Error for GmtimeError {}

/// Floor division: rounds towards negative infinity, which is what a calendar needs.
///
/// Rust's `/` truncates towards zero, so `-1 / 86400` is `0` and a timestamp one second before the
/// epoch would land on day 0 — 1970-01-01 — instead of the day before it. Every pre-1970 date
/// would be off by one day, and only pre-1970 ones, which is the shape of bug that ships.
fn floor_div(numerator: i64, denominator: i64) -> i64 {
    let quotient = numerator / denominator;
    if numerator % denominator != 0 && ((numerator < 0) != (denominator < 0)) {
        quotient - 1
    } else {
        quotient
    }
}

/// Whether `year` is a leap year in the proleptic Gregorian calendar.
fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days before the first of each month in a non-leap year.
const DAYS_BEFORE_MONTH: [i32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

/// Break a Unix timestamp down into UTC.
///
/// # Errors
///
/// [`GmtimeError::YearOutOfRange`] when the year does not fit `int tm_year`. Every other `i64` is
/// converted; there is no other failure.
pub fn gmtime(timestamp: i64) -> Result<Tm, GmtimeError> {
    let days = floor_div(timestamp, SECONDS_PER_DAY);
    // `rem_euclid` rather than `timestamp - days * SECONDS_PER_DAY`, which is the same value and
    // **overflows**: for `timestamp` near `i64::MIN` the floor pushes `days * SECONDS_PER_DAY`
    // past `i64::MIN`, which panics in a debug build and wraps in a release one. Both are
    // reachable from a guest-supplied `time_t`, and a panic reachable from guest input is
    // Critical. `rem_euclid` cannot overflow: its result is in `0..SECONDS_PER_DAY` by
    // construction, and it is the same non-negative remainder the floor division implies.
    let second_of_day = timestamp.rem_euclid(SECONDS_PER_DAY);

    // `civil_from_days`, shifted so the year starts in March and the leap day is last.
    let shifted = days + DAYS_ERA_TO_UNIX_EPOCH;
    let era = floor_div(shifted, DAYS_PER_ERA);
    let day_of_era = shifted - era * DAYS_PER_ERA; // 0..=146_096
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // 0..=399
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // 0..=365
    let shifted_month = (5 * day_of_year + 2) / 153; // 0..=11, 0 is March
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1; // 1..=31
    let month = shifted_month + if shifted_month < 10 { 3 } else { -9 }; // 1..=12
    if month <= 2 {
        year += 1;
    }

    // `tm_year` is years since 1900, and that subtraction is where the range is lost.
    let tm_year = year - 1900;
    let Ok(tm_year) = i32::try_from(tm_year) else {
        return Err(GmtimeError::YearOutOfRange { year });
    };

    // `tm_wday`: 1970-01-01 was a Thursday. `rem_euclid` rather than `%` because `days` is
    // negative for every pre-1970 timestamp and `%` would give a negative weekday.
    let wday = (days + UNIX_EPOCH_WEEKDAY).rem_euclid(7);

    // `tm_yday` from the real (unshifted) month and day.
    let month_index = (month - 1) as usize; // 0..=11, checked by construction above
    let leap_day = i32::from(is_leap(year) && month > 2);
    let yday = DAYS_BEFORE_MONTH[month_index] + (day as i32) - 1 + leap_day;

    Ok(Tm {
        sec: (second_of_day % 60) as i32,
        min: ((second_of_day / 60) % 60) as i32,
        hour: (second_of_day / 3600) as i32,
        mday: day as i32,
        mon: month_index as i32,
        year: tm_year,
        wday: wday as i32,
        yday,
        isdst: 0,
    })
}

/// Serialise `tm` into the 56 bytes of a guest `struct tm`.
///
/// `zone` is a guest pointer to a NUL-terminated `"UTC"`; the caller owns that storage, because a
/// crate with no memory of its own cannot. `tm_gmtoff` is 0, which is what UTC's offset is.
///
/// The 56 bytes are assembled host-side and handed to **one** [`GuestMemory::write`] call rather
/// than eleven. That is not cosmetic: the adapter's implementation of the trait does one bounds
/// check and one `copy_nonoverlapping`, so a destination that is not fully writable faults with
/// nothing written — whereas eleven field writes would leave the guest a `struct tm` that is half
/// this timestamp and half whatever was there before, after a call that reported failure and so
/// gave nobody a reason to look. (The unit-test mock writes byte-at-a-time and does not have that
/// property; it is a property of the boundary, asserted in `omni-android`'s hostile suite.)
///
/// # Errors
///
/// [`Fault`] if the 56 bytes at `at` are not writable guest memory.
pub fn write_tm(
    mem: &mut impl GuestMemory,
    at: u64,
    tm: &Tm,
    zone: u64,
) -> Result<(), Fault> {
    checked_range(at, TM_BYTES as u64)?;
    let mut bytes = [0u8; TM_BYTES];
    let fields = [
        tm.sec, tm.min, tm.hour, tm.mday, tm.mon, tm.year, tm.wday, tm.yday, tm.isdst,
    ];
    for (index, value) in fields.iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    // Offset 36 is the padding the `long` forces, and it stays zero.
    bytes[TM_GMTOFF_OFFSET..TM_GMTOFF_OFFSET + 8].copy_from_slice(&0i64.to_le_bytes());
    bytes[TM_ZONE_OFFSET..TM_ZONE_OFFSET + 8].copy_from_slice(&zone.to_le_bytes());
    mem.write(at, &bytes)
}

/// Read a guest `time_t`.
///
/// `time_t` is a signed 64-bit integer on LP64 Android, so this is the whole of it — there is no
/// 32-bit `time_t` on arm64 and therefore no 2038 problem to model.
///
/// # Errors
///
/// [`Fault`] if the eight bytes at `at` are not readable guest memory.
pub fn read_time_t(mem: &impl GuestMemory, at: u64) -> Result<i64, Fault> {
    checked_range(at, 8)?;
    let mut bytes = [0u8; 8];
    mem.read(at, &mut bytes)?;
    Ok(i64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockMemory;

    /// Known timestamps, converted field by field.
    ///
    /// Each row is `(timestamp, year, mon, mday, hour, min, sec, wday, yday)` and every one is a
    /// date whose weekday is independently checkable. The epoch, a leap day, the day before the
    /// epoch, the last second of a century that is *not* a leap year, and one pre-Gregorian-reform
    /// date to pin that this is the **proleptic** calendar.
    #[test]
    fn known_timestamps_convert_field_by_field() {
        /// One row: a timestamp and the UTC calendar date it is, checked field by field.
        struct Row {
            timestamp: i64,
            /// The calendar year, not `tm_year` — so the `- 1900` is checked too.
            year: i64,
            /// The month 1-12, not `tm_mon` — so the `- 1` is checked too.
            month: i32,
            day: i32,
            hour: i32,
            minute: i32,
            second: i32,
            wday: i32,
            yday: i32,
        }
        let row = |timestamp, year, month, day, hour, minute, second, wday, yday| Row {
            timestamp, year, month, day, hour, minute, second, wday, yday
        };
        let rows: &[Row] = &[
            // 1970-01-01T00:00:00Z, a Thursday.
            row(0, 1970, 1, 1, 0, 0, 0, 4, 0),
            // One second before the epoch: 1969-12-31T23:59:59Z, a Wednesday. This is the row the
            // truncating-division bug fails.
            row(-1, 1969, 12, 31, 23, 59, 59, 3, 364),
            // 2000-02-29T12:00:00Z — a leap day in a year divisible by 400. A Tuesday.
            row(951_825_600, 2000, 2, 29, 12, 0, 0, 2, 59),
            // 2100-03-01T00:00:00Z: 2100 is divisible by 100 and not by 400, so it is NOT a leap
            // year and yday is 59, not 60. A Monday.
            row(4_107_542_400, 2100, 3, 1, 0, 0, 0, 1, 59),
            // 2026-09-10T00:26:40Z, a Thursday.
            row(1_789_000_000, 2026, 9, 10, 0, 26, 40, 4, 252),
            // 1900-01-01T00:00:00Z: tm_year is 0 here, the origin of the field. A Monday.
            row(-2_208_988_800, 1900, 1, 1, 0, 0, 0, 1, 0),
            // 1582-10-14T00:00:00Z. The Gregorian calendar did not exist yet and the reform
            // deleted the ten days around here, so this date is *proleptic* — which is exactly
            // what this algorithm computes and what a `time_t` means. A Thursday.
            row(-12_219_379_200, 1582, 10, 14, 0, 0, 0, 4, 286),
        ];
        for r in rows {
            let at = r.timestamp;
            let tm = gmtime(at).unwrap_or_else(|e| panic!("{at}: {e}"));
            assert_eq!(i64::from(tm.year) + 1900, r.year, "year of {at}");
            assert_eq!(tm.mon + 1, r.month, "month of {at}");
            assert_eq!(tm.mday, r.day, "day of {at}");
            assert_eq!(tm.hour, r.hour, "hour of {at}");
            assert_eq!(tm.min, r.minute, "minute of {at}");
            assert_eq!(tm.sec, r.second, "second of {at}");
            assert_eq!(tm.wday, r.wday, "weekday of {at}");
            assert_eq!(tm.yday, r.yday, "day of year of {at}");
            assert_eq!(tm.isdst, 0, "UTC has no daylight saving");
        }
    }

    /// The weekday advances by exactly one per day across the epoch, in both directions.
    ///
    /// n = 4,000 consecutive days centred on 1970-01-01. A *structural* assertion — it needs no
    /// almanac and it catches the sign bug that only shows up before the epoch, which the table
    /// above covers with one row and this covers with two thousand.
    #[test]
    fn the_weekday_advances_by_one_per_day_across_the_epoch() {
        let mut previous = gmtime(-2_000 * SECONDS_PER_DAY).expect("in range").wday;
        for day in -1_999..2_000i64 {
            let wday = gmtime(day * SECONDS_PER_DAY).expect("in range").wday;
            assert_eq!(wday, (previous + 1) % 7, "day {day}");
            previous = wday;
        }
    }

    /// `tm_yday` is consistent with the month and day it was computed beside.
    ///
    /// n = 40 years of daily timestamps (1990-2030), which covers ten leap years and the
    /// non-leap century boundary is covered by the table above. The check is independent: it
    /// re-derives the day of year by counting, so a wrong `DAYS_BEFORE_MONTH` entry or a wrong
    /// leap-day adjustment cannot agree with it.
    #[test]
    fn the_day_of_year_agrees_with_the_month_and_day() {
        let start = 631_152_000i64; // 1990-01-01T00:00:00Z
        let mut expected_yday = 0i32;
        let mut previous_year = gmtime(start).expect("in range").year;
        for day in 0..(40 * 366) {
            let tm = gmtime(start + day * SECONDS_PER_DAY).expect("in range");
            if tm.year != previous_year {
                expected_yday = 0;
                previous_year = tm.year;
            }
            assert_eq!(tm.yday, expected_yday, "{tm:?}");
            expected_yday += 1;
        }
    }

    /// **No `i64` makes this panic or wrap**, which is the whole of Global Constraint 11 for a
    /// function whose only argument is a number the guest chose.
    ///
    /// Found by the mutation harness rather than by this suite: `gmtime(i64::MIN)` used to compute
    /// `timestamp - days * SECONDS_PER_DAY`, and for timestamps near `i64::MIN` the floor pushes
    /// that product past `i64::MIN`. In a **debug** build it panicked; in a **release** build it
    /// wrapped and produced a time of day from nonsense. The whole workspace suite runs
    /// `--release`, which is exactly why it never saw it.
    ///
    /// The boundaries are enumerated rather than sampled: the two extremes, the two values one
    /// step inside them, and the two day boundaries nearest `i64::MIN`, because the overflow is a
    /// property of the multiplication rather than of any particular date.
    #[test]
    fn no_timestamp_at_all_can_make_this_panic_or_wrap() {
        let edges = [
            i64::MIN,
            i64::MIN + 1,
            i64::MIN + SECONDS_PER_DAY,
            -SECONDS_PER_DAY - 1,
            -1,
            0,
            1,
            i64::MAX - SECONDS_PER_DAY,
            i64::MAX - 1,
            i64::MAX,
        ];
        for timestamp in edges {
            // Either a `Tm` whose fields are in range, or the one documented refusal. Never a
            // panic, and never a time of day outside a day.
            match gmtime(timestamp) {
                Ok(tm) => {
                    assert!((0..24).contains(&tm.hour), "{timestamp}: hour {}", tm.hour);
                    assert!((0..60).contains(&tm.min), "{timestamp}: minute {}", tm.min);
                    assert!((0..60).contains(&tm.sec), "{timestamp}: second {}", tm.sec);
                    assert!((0..7).contains(&tm.wday), "{timestamp}: weekday {}", tm.wday);
                    assert!((0..366).contains(&tm.yday), "{timestamp}: day of year {}", tm.yday);
                    assert!((1..=31).contains(&tm.mday), "{timestamp}: day {}", tm.mday);
                    assert!((0..12).contains(&tm.mon), "{timestamp}: month {}", tm.mon);
                }
                Err(GmtimeError::YearOutOfRange { .. }) => {}
            }
        }
    }

    /// A year that does not fit `int tm_year` is refused, not wrapped.
    #[test]
    fn a_year_that_does_not_fit_an_int_is_refused() {
        // i64::MAX seconds is about year 292,277,026,596 — comfortably past an i32 of years.
        let error = gmtime(i64::MAX).expect_err("the year cannot fit an int");
        assert!(matches!(error, GmtimeError::YearOutOfRange { .. }), "{error}");
        let error = gmtime(i64::MIN).expect_err("the year cannot fit an int");
        assert!(matches!(error, GmtimeError::YearOutOfRange { .. }), "{error}");
        // The boundary is exact, and it is the *only* boundary: `tm_year` is `i32::MAX` in the
        // calendar year 2,147,485,547, so its last second converts and the next one does not.
        let last = 67_768_036_191_676_799i64; // 2147485547-12-31T23:59:59Z
        let tm = gmtime(last).expect("the largest year that fits tm_year");
        assert_eq!(tm.year, i32::MAX, "tm_year at its maximum");
        assert_eq!(i64::from(tm.year) + 1900, 2_147_485_547);
        assert_eq!(tm.mon, 11);
        assert_eq!(tm.mday, 31);
        assert_eq!((tm.hour, tm.min, tm.sec), (23, 59, 59));
        assert!(
            matches!(gmtime(last + 1), Err(GmtimeError::YearOutOfRange { year: 2_147_485_548 })),
            "one second past the last representable year must refuse, not wrap"
        );
    }

    /// Every second of a day maps to a distinct `(hour, min, sec)`, and the day rolls at midnight.
    ///
    /// n = 86,400 consecutive seconds. The failure this catches is an hour computed with the wrong
    /// divisor, which is correct at 00:00 and wrong everywhere else.
    #[test]
    fn every_second_of_a_day_is_distinct_and_the_day_rolls_at_midnight() {
        let midnight = 1_767_225_600i64; // 2026-01-01T00:00:00Z
        let mut seen = std::collections::BTreeSet::new();
        for offset in 0..SECONDS_PER_DAY {
            let tm = gmtime(midnight + offset).expect("in range");
            assert_eq!(tm.mday, 1, "the day rolled early at +{offset}");
            assert!(seen.insert((tm.hour, tm.min, tm.sec)), "duplicate time at +{offset}");
            assert!((0..24).contains(&tm.hour) && (0..60).contains(&tm.min));
            assert!((0..60).contains(&tm.sec), "a time_t has no leap seconds in it");
        }
        assert_eq!(seen.len(), SECONDS_PER_DAY as usize);
        assert_eq!(gmtime(midnight + SECONDS_PER_DAY).expect("in range").mday, 2);
    }

    /// The 56 bytes land where the layout says, and a short mapping faults instead of writing half.
    #[test]
    fn the_struct_is_written_in_one_piece_at_the_documented_offsets() {
        let mut mem = MockMemory::new();
        mem.map(0x1000, &[0u8; TM_BYTES]);
        let tm = gmtime(0).expect("the epoch");
        write_tm(&mut mem, 0x1000, &tm, 0xDEAD_BEEF).expect("a mapped struct tm");
        let mut bytes = [0u8; TM_BYTES];
        mem.read(0x1000, &mut bytes).expect("read it back");
        // 1970-01-01: sec/min/hour/mon/yday 0, mday 1, year 70, wday 4.
        assert_eq!(i32::from_le_bytes(bytes[12..16].try_into().unwrap()), 1, "tm_mday");
        assert_eq!(i32::from_le_bytes(bytes[20..24].try_into().unwrap()), 70, "tm_year");
        assert_eq!(i32::from_le_bytes(bytes[24..28].try_into().unwrap()), 4, "tm_wday");
        assert_eq!(&bytes[36..40], &[0; 4], "the padding before tm_gmtoff");
        assert_eq!(
            i64::from_le_bytes(bytes[TM_GMTOFF_OFFSET..TM_GMTOFF_OFFSET + 8].try_into().unwrap()),
            0,
            "UTC's offset is zero"
        );
        assert_eq!(
            u64::from_le_bytes(bytes[TM_ZONE_OFFSET..TM_ZONE_OFFSET + 8].try_into().unwrap()),
            0xDEAD_BEEF,
            "tm_zone"
        );

        // One byte short of the structure: the write fails rather than running off the end.
        let mut short = MockMemory::new();
        short.map(0x2000, &[0u8; TM_BYTES - 1]);
        assert!(write_tm(&mut short, 0x2000, &tm, 0).is_err(), "a short mapping must fault");
    }

    /// A null or wrapping `struct tm` pointer is a fault, not a write.
    #[test]
    fn a_hostile_destination_is_a_fault() {
        let mut mem = MockMemory::new();
        mem.map(0x1000, &[0u8; TM_BYTES]);
        let tm = gmtime(0).expect("the epoch");
        assert_eq!(write_tm(&mut mem, 0, &tm, 0), Err(Fault(0)), "a null struct tm");
        assert!(write_tm(&mut mem, u64::MAX - 8, &tm, 0).is_err(), "a wrapping struct tm");
        assert_eq!(read_time_t(&mem, 0), Err(Fault(0)), "a null time_t");
        assert!(read_time_t(&mem, u64::MAX - 2).is_err(), "a wrapping time_t");
    }

    /// `floor_div` rounds towards negative infinity, which `/` does not.
    #[test]
    fn floor_division_rounds_towards_negative_infinity() {
        assert_eq!(floor_div(-1, SECONDS_PER_DAY), -1, "-1 / 86400 truncates to 0, which is wrong");
        assert_eq!(floor_div(-SECONDS_PER_DAY, SECONDS_PER_DAY), -1);
        assert_eq!(floor_div(-SECONDS_PER_DAY - 1, SECONDS_PER_DAY), -2);
        assert_eq!(floor_div(0, SECONDS_PER_DAY), 0);
        assert_eq!(floor_div(SECONDS_PER_DAY - 1, SECONDS_PER_DAY), 0);
    }

    /// The Gregorian leap rule, including both century cases.
    #[test]
    fn the_leap_rule_is_gregorian_and_not_julian() {
        assert!(is_leap(2024) && is_leap(2000) && is_leap(1600));
        assert!(!is_leap(1900) && !is_leap(2100) && !is_leap(2023));
        // Negative (proleptic) years follow the same rule; year 0 is a leap year.
        assert!(is_leap(0) && is_leap(-400));
        assert!(!is_leap(-100));
    }
}
