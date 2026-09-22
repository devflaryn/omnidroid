//! `gmtime_r`: a Unix timestamp as broken-down UTC, and `strftime`: broken-down time as text.
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
//! one reason that matters to this project: it is **loop-free — O(1) over the whole `i64` range**,
//! so a hostile `time_t` cannot make it slow. (It was described here as *branch-free*, which is
//! literally untrue — `floor_div` has an `if`/`else` and `gmtime` has two more. The property that
//! matters is the absence of an accumulating loop, and nothing should lean on the stronger word:
//! this is not a constant-time algorithm and is not written to be one.) A "step a year at a time from
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

//! # `strftime`, and why it is here rather than in the adapter
//!
//! `strftime` is the other half of the same shape: it needs no clock, no timezone database and
//! no locale table beyond the one bionic has (the C locale — see [`crate::locale`]). What it
//! needs is a `struct tm` and a format string, both of which the caller already holds, so all
//! that is left is text assembly over integers.
//!
//! It is written here **whole or not at all**, per conversion. `docs/HANDOFF.md` records why:
//! it is "a full C library function with a format language, and a partial one is the
//! plausible-stub shape — an unimplemented specifier produces wrong *text*, which nothing
//! downstream can tell from right text". So every conversion this module cannot derive from C99
//! or POSIX is a named [`StrftimeError`], and none of them is copied through as literal text the
//! way a tzcode-derived `strftime` copies an unknown one. [`strftime`]'s own documentation lists
//! the implemented set and every refusal by name.
//!
//! Two of its conversions read fields that are **not** POSIX: `%z` reads `tm_gmtoff` and `%Z`
//! reads `tm_zone`, the two BSD extensions at the end of bionic's `struct tm`. The layout they
//! live at is [`TM_BYTES`]'s, which was derived for `gmtime` and is reused rather than restated —
//! two derivations of one layout are two chances to disagree about offset 40.

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

/// The inverse of [`gmtime`]: a broken-down UTC time as a Unix timestamp, normalised.
///
/// This is `timegm(3)`, and on this runtime it is also `mktime(3)` -- see the adapter's
/// `mktime` handler for why the two coincide here and would not on a device with a timezone.
///
/// # What "normalised" means, and why it is the whole of the contract
///
/// C 7.29.2.3 says `mktime` interprets a `struct tm` whose members **need not be in their normal
/// ranges** and, on success, sets them to values that are. So the 61st second of a minute is the
/// first second of the next one, month 12 is January of the following year, and day 0 is the last
/// day of the previous month. Callers rely on exactly that: OpenSSL builds a `struct tm` from an
/// X.509 `notBefore`/`notAfter` string and lets this function decide what it means.
///
/// The arithmetic is the mirror of [`gmtime`]'s and is **loop-free for the same reason**: a
/// "step a month at a time" normalisation over a hostile `tm_mon` is a denial of service
/// reachable from one guest argument. Months normalise by division, and the day count is
/// `days_from_civil` -- Howard Hinnant's exact inverse of the `civil_from_days` [`gmtime`] uses,
/// from the same paper C++20's `<chrono>` is specified against, so the pair round-trips by
/// construction rather than by agreement.
///
/// `tm_wday` and `tm_yday` in the input are **ignored**, which is what C requires; the returned
/// [`Tm`] carries the ones the date implies. `tm_isdst` is ignored too: UTC has no daylight
/// saving, so there is no third answer for `-1` to ask for.
///
/// # Errors
///
/// [`MktimeError::OutOfRange`] when the normalised date does not fit a `time_t`, which C reports
/// as `-1`. Every arithmetic step below is `checked_*`: a `struct tm` is nine guest-controlled
/// `int`s, `tm_year` alone can be `i32::MIN`, and this is exactly the shape `VERIFICATION.md`
/// entry 3 is about -- a release build wraps, the wrapped value passes a later range check, and
/// the answer is a plausible date.
pub fn mktime(tm: &Tm) -> Result<(i64, Tm), MktimeError> {
    let fail = || MktimeError::OutOfRange { year: i64::from(tm.year), mon: i64::from(tm.mon) };

    // **Months first**, because normalising them is what decides the year. `tm_mon` is 0-based,
    // so a floor division by twelve carries into the year and the remainder is the month.
    let months = i64::from(tm.year).checked_mul(12).ok_or_else(fail)?
        .checked_add(i64::from(tm.mon)).ok_or_else(fail)?;
    let year = floor_div(months, 12).checked_add(1900).ok_or_else(fail)?;
    let month = months - floor_div(months, 12) * 12; // 0..=11
    let month = month + 1; // 1..=12, which is what `days_from_civil` takes

    // `days_from_civil` takes the day of month as an `i64` and tolerates any value: an out-of-
    // range `tm_mday` simply moves the day count, which is the normalisation C asks for.
    let days = days_from_civil(year, month, 1)
        .checked_add(i64::from(tm.mday))
        .ok_or_else(fail)?
        .checked_sub(1)
        .ok_or_else(fail)?;

    let seconds = days
        .checked_mul(SECONDS_PER_DAY)
        .ok_or_else(fail)?
        .checked_add(i64::from(tm.hour).checked_mul(3600).ok_or_else(fail)?)
        .ok_or_else(fail)?
        .checked_add(i64::from(tm.min).checked_mul(60).ok_or_else(fail)?)
        .ok_or_else(fail)?
        .checked_add(i64::from(tm.sec))
        .ok_or_else(fail)?;

    // **The normalised fields come from `gmtime` rather than from this function's own
    // arithmetic**, and that is deliberate: two independent normalisations that were meant to
    // agree are two places for them to disagree, and the one a caller will compare against is
    // whatever `gmtime` says about the timestamp it was just handed.
    let normalised = gmtime(seconds).map_err(|_| fail())?;
    Ok((seconds, normalised))
}

/// Why a `struct tm` could not be converted to a `time_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MktimeError {
    /// The date is outside the range a `time_t` holds.
    ///
    /// C's answer is `(time_t)-1`. Wrapping instead would produce a timestamp, which is the same
    /// failure shape [`GmtimeError::YearOutOfRange`] exists to avoid in the other direction.
    OutOfRange {
        /// The year the fields implied, for the message.
        year: i64,
        /// The month, 0-based as `tm_mon` is.
        mon: i64,
    },
}

impl core::fmt::Display for MktimeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MktimeError::OutOfRange { year, mon } => write!(
                f,
                "the broken-down time (tm_year implying {year}, tm_mon {mon}) is outside the \
                 range a 64-bit time_t holds, which C reports as (time_t)-1"
            ),
        }
    }
}

impl std::error::Error for MktimeError {}

/// Days since 1970-01-01 for a proleptic-Gregorian date: `days_from_civil`.
///
/// **The exact inverse of the `civil_from_days` in [`gmtime`]**, from the same paper, so that
/// `gmtime(mktime(t)) == t` holds by construction. `month` is 1-based here, unlike `tm_mon`.
/// `day` is not range-checked: a value outside 1..=31 moves the result, which is the
/// normalisation `mktime` needs and the reason this takes an `i64`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // Shift the year so it starts in March and the leap day is last, exactly as the inverse does.
    let year = if month <= 2 { year - 1 } else { year };
    let era = floor_div(year, 400);
    let year_of_era = year - era * 400; // 0..=399
    let shifted_month = if month > 2 { month - 3 } else { month + 9 }; // 0..=11, 0 is March
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1; // 0..=365
    let day_of_era =
        year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year; // 0..=146_096
    era * DAYS_PER_ERA + day_of_era - DAYS_ERA_TO_UNIX_EPOCH
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

/// The most bytes one [`strftime`] call will produce before it refuses.
///
/// **A policy number, stated as one**, and the companion to [`crate::printf::MAX_OUTPUT`].
/// `strftime`'s expansion factor is bounded — the widest specifier this implementation emits is
/// `%c`, which is 24 bytes from a 2-byte specification, so a 64 KiB format string (the thunk
/// boundary's `STRING_LIMIT`) cannot produce more than about 768 KiB. The cap is above that on
/// purpose: it is not reachable from the boundary, it is reachable from *this function's own
/// argument*, which is a `&[u8]` any caller may make as long as it likes. Without it a caller
/// could ask for an allocation it did not size, and an allocation failure aborts.
///
/// One specifier is **not** bounded by the format string: `%Z` emits `tm_zone`'s bytes, whose
/// length the caller chose. The cap is therefore checked once per format-string byte and the
/// overshoot is bounded by one zone name, which is stated here rather than silently true.
pub const MAX_STRFTIME_OUTPUT: usize = 1024 * 1024;

/// Abbreviated weekday names, `%a`, Sunday first because `tm_wday` counts Sunday as 0.
///
/// POSIX's C locale fixes these exactly (`abday` in the POSIX locale definition). A believable
/// wrong answer would have been the host's names, which are the host's locale's.
const WEEKDAY_ABBREVIATED: [&[u8]; 7] =
    [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];

/// Full weekday names, `%A`. POSIX C locale `day`.
const WEEKDAY_FULL: [&[u8]; 7] = [
    b"Sunday",
    b"Monday",
    b"Tuesday",
    b"Wednesday",
    b"Thursday",
    b"Friday",
    b"Saturday",
];

/// Abbreviated month names, `%b` and `%h`. POSIX C locale `abmon`.
const MONTH_ABBREVIATED: [&[u8]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
    b"Dec",
];

/// Full month names, `%B`. POSIX C locale `mon`.
const MONTH_FULL: [&[u8]; 12] = [
    b"January",
    b"February",
    b"March",
    b"April",
    b"May",
    b"June",
    b"July",
    b"August",
    b"September",
    b"October",
    b"November",
    b"December",
];

/// A `struct tm` as [`strftime`] reads it: the nine POSIX fields plus the two BSD extensions
/// bionic's `struct tm` carries.
///
/// The fields are the **guest's** and are not validated on construction — `tm_year` may be any
/// `i32`, `tm_mon` may be 10,000 and `tm_wday` may be -5. Each specifier checks the fields it
/// actually reads, so a `tm` that is nonsense in a field nobody looks at still formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrftimeTm<'a> {
    /// The nine POSIX fields, in [`Tm`]'s spelling of them.
    pub tm: Tm,
    /// `tm_gmtoff`: seconds **east** of UTC. Read by `%z` and by nothing else.
    ///
    /// `i64` because the guest field is a `long` and LP64's `long` is 64 bits. Narrowing it to
    /// `i32` here would silently reinterpret half of the values a guest can store.
    pub gmtoff: i64,
    /// `tm_zone`: the zone abbreviation's bytes, or `None` when the guest's pointer was NULL.
    ///
    /// The bytes, not a `&str`: this is guest memory and nothing guarantees it is UTF-8. Read by
    /// `%Z` and by nothing else.
    pub zone: Option<&'a [u8]>,
}

/// What [`strftime`] produced, in the two shapes C's return value distinguishes.
///
/// # Why this is an enum and not a `usize`
///
/// C99 7.23.3.5: `strftime` returns the number of bytes written, not counting the terminating
/// NUL — **or zero, in which case "the contents of the array are indeterminate"**. That is not
/// the same as "wrote a truncated string": a caller that treats a zero return as truncation
/// reads bytes the standard does not define. Returning a `usize` alone would let that mistake
/// happen silently; returning [`StrftimeOutput::DoesNotFit`] with **no text in it at all** makes
/// it impossible — there is nothing to copy, so nothing can be copied by accident.
///
/// Note that `strftime` also returns 0 when the format string legitimately produces an empty
/// result and `max >= 1`. That case is [`StrftimeOutput::Fits`] with an empty `Vec`, and it is
/// the reason C's own interface is famously unable to distinguish the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrftimeOutput {
    /// The result and its NUL fit in `max`. The `Vec` is the bytes **without** the NUL, and its
    /// length is what C returns.
    Fits(Vec<u8>),
    /// The result including its NUL would not fit in `max`. C returns **0** here and leaves the
    /// destination buffer indeterminate; the caller must not write anything to the guest.
    DoesNotFit {
        /// How many bytes the result would have been, not counting the NUL. So `needed + 1` is
        /// the `max` that would have been enough. Diagnostic only — C gives the caller no way to
        /// learn this.
        needed: usize,
    },
}

/// Why [`strftime`] refused.
///
/// Every variant **names** what could not be done. There is no variant that means "emitted
/// something plausible": a specifier this implementation cannot produce correctly is an error,
/// not literal text, because an unimplemented specifier's wrong output is indistinguishable
/// downstream from right output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrftimeError {
    /// The format string ended inside a conversion specification: a trailing `%`, or a trailing
    /// `%E`/`%O` with nothing after the modifier.
    IncompleteSpecifier,
    /// A conversion this implementation does not know.
    ///
    /// C99 7.23.3.5 makes any specifier outside its list **undefined behaviour**, and
    /// implementations differ: tzcode-derived libraries copy the two bytes through, others drop
    /// them. Copying them through is the tempting choice and it is the one this crate forbids —
    /// the guest asked for a time and got the literal text `%Q`, which no downstream check can
    /// tell from a time.
    UnknownSpecifier {
        /// The byte that followed the `%`.
        specifier: u8,
    },
    /// A conversion that exists in GNU/BSD `strftime` but in neither C99 nor POSIX.
    ///
    /// Refused rather than implemented because the only specification for it is another
    /// implementation's documentation, and **whether bionic accepts it has not been verified
    /// here**. Both possible bionic behaviours — expanding it, or copying it through literally —
    /// produce text this crate cannot confirm, so it produces none.
    ExtensionSpecifier {
        /// The byte that followed the `%`.
        specifier: u8,
        /// What that extension means where it exists.
        what: &'static str,
    },
    /// `%s`, seconds since the Epoch, which is `mktime()` of the broken-down time.
    ///
    /// **Not implementable here, and the near miss is worth naming.** `timegm(tm) - tm_gmtoff`
    /// looks like the answer and is only the answer when two things the guest controls are
    /// already true: that the fields are normalised (`mktime` normalises `tm_mon = 14` into the
    /// next year, and the caller may not have) and that `tm_gmtoff` is the offset genuinely in
    /// force at that instant rather than an arbitrary `long`. `mktime` resolves both from the
    /// timezone database, which this crate has no access to by design (D19).
    SecondsSinceEpochUnavailable,
    /// A `struct tm` field a specifier reads is outside the range POSIX `<time.h>` fixes for it.
    ///
    /// The fields come from the guest, so this is the expected case rather than an internal
    /// error. Refused per specifier and named per field: `%d` refuses a `tm_mday` of 0 and says
    /// so, while `%Y` beside it does not care.
    FieldOutOfRange {
        /// The conversion that read the field.
        specifier: u8,
        /// The field's C name, e.g. `"tm_mday"`.
        field: &'static str,
        /// What the guest put there.
        value: i32,
        /// The lowest value POSIX allows.
        lower: i32,
        /// The highest value POSIX allows.
        upper: i32,
    },
    /// A year specifier was asked for a year outside the range whose representation the standard
    /// fixes. See [`strftime`]'s "Years outside \[1000, 9999\]" section for the whole argument.
    YearNotRepresentable {
        /// The conversion that asked.
        specifier: u8,
        /// The proleptic Gregorian year, i.e. `tm_year + 1900` (or the ISO week-based year for
        /// `%G` and `%g`).
        year: i64,
        /// The lowest year this specifier can represent.
        lower: i64,
        /// The highest year this specifier can represent.
        upper: i64,
    },
    /// `%Z` was asked for the zone abbreviation and the guest's `tm_zone` pointer was NULL.
    ///
    /// A `struct tm` with no `tm_zone` sends a real `strftime` to the global `tzname`, which is
    /// timezone state this crate does not have and must not invent. Emitting `"UTC"` would be
    /// the plausible stub: correct for every `tm` this project currently produces and wrong the
    /// moment one of them is not UTC.
    TimeZoneNameUnavailable,
    /// `%z` was asked for a `tm_gmtoff` outside the range a UTC offset can occupy.
    ///
    /// POSIX's `TZ` grammar bounds an offset's hour field at 24, so the representable range is
    /// ±86,400 seconds. Outside it `+hhmm` has no meaning: the hours field would need more than
    /// two digits, and printing more digits invents a format nothing parses.
    GmtoffOutOfRange {
        /// What the guest put in `tm_gmtoff`.
        gmtoff: i64,
    },
    /// The result passed [`MAX_STRFTIME_OUTPUT`].
    OutputTooLarge {
        /// The cap that was passed.
        limit: usize,
    },
}

impl core::fmt::Display for StrftimeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StrftimeError::IncompleteSpecifier => {
                write!(f, "strftime: the format string ended inside a conversion specification")
            }
            StrftimeError::UnknownSpecifier { specifier } => write!(
                f,
                "strftime: '%{}' is not a C99 or POSIX conversion, and this implementation does \
                 not copy an unknown conversion through as literal text",
                DisplayByte(*specifier)
            ),
            StrftimeError::ExtensionSpecifier { specifier, what } => write!(
                f,
                "strftime: '%{}' ({what}) is outside C99 and POSIX and is refused rather than \
                 guessed",
                DisplayByte(*specifier)
            ),
            StrftimeError::SecondsSinceEpochUnavailable => write!(
                f,
                "strftime: '%s' is mktime() of the broken-down time, which needs field \
                 normalisation and the timezone database — neither is available here"
            ),
            StrftimeError::FieldOutOfRange { specifier, field, value, lower, upper } => write!(
                f,
                "strftime: '%{}' reads {field}, which POSIX bounds to [{lower}, {upper}] and the \
                 caller set to {value}",
                DisplayByte(*specifier)
            ),
            StrftimeError::YearNotRepresentable { specifier, year, lower, upper } => write!(
                f,
                "strftime: '%{}' has no specified representation for the year {year}; it is \
                 defined over [{lower}, {upper}]",
                DisplayByte(*specifier)
            ),
            StrftimeError::TimeZoneNameUnavailable => write!(
                f,
                "strftime: '%Z' needs tm_zone or the global tzname, and tm_zone was NULL"
            ),
            StrftimeError::GmtoffOutOfRange { gmtoff } => write!(
                f,
                "strftime: '%z' formats a UTC offset as +hhmm and tm_gmtoff is {gmtoff} seconds, \
                 outside the ±86400 POSIX allows a TZ offset"
            ),
            StrftimeError::OutputTooLarge { limit } => {
                write!(f, "strftime: the result passed the {limit}-byte cap")
            }
        }
    }
}

impl std::error::Error for StrftimeError {}

/// Renders one format byte for an error message: itself when it is printable ASCII, `\xNN`
/// otherwise, so `%\x00` does not put a NUL in a log line.
struct DisplayByte(u8);

impl core::fmt::Display for DisplayByte {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.is_ascii_graphic() {
            write!(f, "{}", self.0 as char)
        } else {
            write!(f, "\\x{:02x}", self.0)
        }
    }
}

/// `size_t strftime(char *s, size_t max, const char *format, const struct tm *tm)` — the
/// formatting half of it, with the guest buffer left to the caller.
///
/// # The return value, exactly
///
/// C99 7.23.3.5 paragraph 4: the function returns the number of bytes placed in the array **not
/// including the terminating null character**, *if the total number of resulting characters
/// including the terminating null character is not more than `maxsize`*; **otherwise zero is
/// returned and the contents of the array are indeterminate.** So the test is
/// `produced + 1 <= max`, which is spelled `produced < max` here because that is the same
/// predicate with no addition to overflow. [`StrftimeOutput`] carries the distinction in its
/// shape; see its documentation for why it is not a `usize`.
///
/// A believable wrong answer would have been to truncate at `max - 1` and return that length,
/// which is `snprintf`'s contract and not this one. It is wrong twice: the return value would be
/// a length rather than zero, and the buffer would hold a prefix of a timestamp that reads
/// exactly like a whole one — `2026-09-2` is a date.
///
/// # Bytes, not characters
///
/// `format` is `&[u8]` and the output is `Vec<u8>` because C counts bytes and a guest format
/// string is not guaranteed to be UTF-8. Taking a `&str` would force the caller to convert, and
/// the only total conversion available is lossy — it would replace an unconvertible byte with
/// U+FFFD, changing bytes that the C function copies through untouched. That is a wrong-text
/// failure introduced by the signature.
///
/// # The `%E` and `%O` modifiers
///
/// C99 7.23.3.5 paragraph 3: "if the alternative format or specification does not exist for the
/// current locale, the modifier is ignored". bionic has exactly one locale's behaviour — the C
/// locale (see [`crate::locale`]) — and the C locale defines no `E` or `O` alternatives, so every
/// `%Ex` is its `%x` and every `%Oy` is its `%y`. This implementation accepts the modifier before
/// any conversion it handles rather than only before the pairs POSIX lists, because the reason
/// they are equivalent — the C locale has no alternative representations at all — does not depend
/// on which conversion follows. Exactly one modifier is consumed: `%EOy` is `%E` followed by the
/// conversion `O`, which is unknown.
///
/// # Which conversions are implemented, and which refuse
///
/// Implemented: `%a %A %b %B %c %C %d %D %e %F %g %G %h %H %I %j %m %M %n %p %r %R %S %t %T %u
/// %U %V %w %W %x %X %y %Y %z %Z %%`, each with its `%E`/`%O` forms. That is the whole of C99 and
/// POSIX except `%s`.
///
/// Refused **by name**, never emitted:
///
/// * `%s` — [`StrftimeError::SecondsSinceEpochUnavailable`], which explains the near miss.
/// * `%k`, `%l`, `%P`, `%v`, `%+` — [`StrftimeError::ExtensionSpecifier`]. GNU/BSD extensions
///   outside C99 and POSIX.
/// * anything else — [`StrftimeError::UnknownSpecifier`].
///
/// # `struct tm` fields outside their POSIX ranges
///
/// POSIX `<time.h>` gives each field a range: `tm_sec` `[0,60]` (the 60 is the leap second),
/// `tm_min` `[0,59]`, `tm_hour` `[0,23]`, `tm_mday` `[1,31]`, `tm_mon` `[0,11]`, `tm_wday`
/// `[0,6]`, `tm_yday` `[0,365]`. `tm_year` is the one field POSIX leaves unbounded.
///
/// A field outside its range is [`StrftimeError::FieldOutOfRange`], **checked by the specifiers
/// that read it and by no others**. Two of those fields index a name table, where an out-of-range
/// value has no text at all; the rest are printed as numbers, where the tempting answer is to
/// print the number anyway. `%S` of a `tm_sec` of -5 would be `-5`, a two-character field where
/// the format promised two digits, and it would flow into a timestamp that still parses.
///
/// # Years outside \[1000, 9999\]
///
/// `%Y` and `%G` are **defined over \[1000, 9999\]** here and refuse outside it; `%C`, `%y` and
/// `%g` are defined over \[0, 9999\].
///
/// The reason is that C99 fixes the *value* of every year conversion and the *width* of only
/// some. `%C` is "the year divided by 100 and truncated to an integer, as a decimal number
/// (00-99)" and `%y` is "the last 2 digits of the year (00-99)": two digits, unambiguous for
/// every year from 0 to 9999, and undefined for a year that has no two-digit century. `%Y` is
/// "the year as a decimal number (for example, 1997)", which fixes no width — so the year 500 is
/// `500` or `0500` depending on the implementation, the year -4 is `-4` or `-0004` or `-004`, and
/// **there is no standard to derive the answer from**. `%G` is worse: ISO 8601 fixes four digits
/// for \[1000, 9999\] and requires a *mutual agreement* between sender and receiver for any
/// expanded representation, which is a protocol this function is not part of.
///
/// A believable wrong answer would have been to zero-pad to four digits and print a leading `-`
/// for negative years. It is believable because two widely used implementations do it, which is
/// exactly the reason it is not evidence (`docs/VERIFICATION.md` entry 7).
///
/// # Errors
///
/// [`StrftimeError`], one variant per refusal. Nothing is ever emitted for a conversion that
/// returns one: the error abandons the whole call rather than leaving a partial result, because
/// a partial timestamp is a timestamp.
pub fn strftime(
    max: usize,
    format: &[u8],
    when: &StrftimeTm<'_>,
) -> Result<StrftimeOutput, StrftimeError> {
    let mut out: Vec<u8> = Vec::new();
    let mut index = 0usize;
    while index < format.len() {
        // Checked here rather than at every push: one iteration appends at most one conversion,
        // and every conversion but `%Z` is a couple of dozen bytes. `%Z` is `tm_zone`, whose
        // length the caller chose, so the overshoot past the cap is bounded by one zone name —
        // stated in MAX_STRFTIME_OUTPUT's documentation rather than left to be discovered.
        if out.len() > MAX_STRFTIME_OUTPUT {
            return Err(StrftimeError::OutputTooLarge { limit: MAX_STRFTIME_OUTPUT });
        }
        let byte = format[index];
        index += 1;
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        let Some(&next) = format.get(index) else {
            return Err(StrftimeError::IncompleteSpecifier);
        };
        index += 1;
        // One `E` or `O` is consumed and discarded: the C locale has no alternative
        // representations, so the modified conversion *is* the unmodified one.
        let specifier = if next == b'E' || next == b'O' {
            let Some(&modified) = format.get(index) else {
                return Err(StrftimeError::IncompleteSpecifier);
            };
            index += 1;
            modified
        } else {
            next
        };
        emit(&mut out, specifier, when)?;
    }
    // `produced + 1 <= max` without the addition. See this function's "return value" section.
    if out.len() < max {
        Ok(StrftimeOutput::Fits(out))
    } else {
        Ok(StrftimeOutput::DoesNotFit { needed: out.len() })
    }
}

/// Emit one conversion.
///
/// # Recursion, and why it needs no depth guard
///
/// The compound conversions (`%c %D %F %r %R %T %x %X`) are defined by the standard *as* other
/// conversions, so they are written that way — one place decides what `%H` looks like, and `%T`
/// cannot drift from it. Every conversion a compound expands to is a **primitive** one, so the
/// recursion is one level deep by construction and a runtime depth counter would be a branch no
/// input can reach (`docs/VERIFICATION.md` entry 12). The property that makes that true is local
/// and checkable: no arm below that recurses names `c`, `D`, `F`, `r`, `R`, `T`, `x` or `X`.
fn emit(out: &mut Vec<u8>, specifier: u8, when: &StrftimeTm<'_>) -> Result<(), StrftimeError> {
    let tm = &when.tm;
    match specifier {
        // ---- names out of the C locale's tables -------------------------------------------
        b'a' => out.extend_from_slice(WEEKDAY_ABBREVIATED[wday(tm, specifier)? as usize]),
        b'A' => out.extend_from_slice(WEEKDAY_FULL[wday(tm, specifier)? as usize]),
        // POSIX: "%h — equivalent to %b". Not "similar to": the same conversion.
        b'b' | b'h' => out.extend_from_slice(MONTH_ABBREVIATED[mon(tm, specifier)? as usize]),
        b'B' => out.extend_from_slice(MONTH_FULL[mon(tm, specifier)? as usize]),

        // ---- the compound conversions, each spelled as the standard defines it -------------
        // POSIX LC_TIME, C locale: d_t_fmt is "%a %b %e %H:%M:%S %Y".
        b'c' => {
            emit(out, b'a', when)?;
            out.push(b' ');
            emit(out, b'b', when)?;
            out.push(b' ');
            emit(out, b'e', when)?;
            out.push(b' ');
            emit(out, b'H', when)?;
            out.push(b':');
            emit(out, b'M', when)?;
            out.push(b':');
            emit(out, b'S', when)?;
            out.push(b' ');
            emit(out, b'Y', when)?;
        }
        // C99: "%D — equivalent to %m/%d/%y". POSIX's C-locale `d_fmt`, which is what %x is,
        // happens to be the same string — so they share an arm. They are equal *in the C
        // locale*, not by definition: %D is fixed by the standard and %x is whatever the locale
        // says, and bionic has one locale (see `crate::locale`).
        b'D' | b'x' => {
            emit(out, b'm', when)?;
            out.push(b'/');
            emit(out, b'd', when)?;
            out.push(b'/');
            emit(out, b'y', when)?;
        }
        // C99: "%F — equivalent to %Y-%m-%d (the ISO 8601 date format)".
        b'F' => {
            emit(out, b'Y', when)?;
            out.push(b'-');
            emit(out, b'm', when)?;
            out.push(b'-');
            emit(out, b'd', when)?;
        }
        // POSIX C locale t_fmt_ampm: "%I:%M:%S %p".
        b'r' => {
            emit(out, b'I', when)?;
            out.push(b':');
            emit(out, b'M', when)?;
            out.push(b':');
            emit(out, b'S', when)?;
            out.push(b' ');
            emit(out, b'p', when)?;
        }
        // C99: "%R — equivalent to %H:%M".
        b'R' => {
            emit(out, b'H', when)?;
            out.push(b':');
            emit(out, b'M', when)?;
        }
        // C99: "%T — equivalent to %H:%M:%S". POSIX's C-locale `t_fmt`, which is what %X is, is
        // the same string, so they share an arm for the same reason %D and %x do.
        b'T' | b'X' => {
            emit(out, b'H', when)?;
            out.push(b':');
            emit(out, b'M', when)?;
            out.push(b':');
            emit(out, b'S', when)?;
        }

        // ---- the numeric conversions ------------------------------------------------------
        // C99 %C: "the year divided by 100 and truncated to an integer, as a decimal number
        // (00-99)". Truncating division, which for the non-negative years this arm accepts is
        // the same as flooring — the distinction only bites below zero, which is refused.
        b'C' => {
            let year = year_within(tm, specifier, 0, 9999)?;
            push_padded(out, (year / 100) as u64, 2, b'0');
        }
        b'd' => push_padded(out, mday(tm, specifier)? as u64, 2, b'0'),
        // C99 %e: "the day of the month as a decimal number (1-31); a single digit is preceded
        // by a space". A space, not a zero — that is the whole difference from %d, and swapping
        // them produces a date that still parses.
        b'e' => push_padded(out, mday(tm, specifier)? as u64, 2, b' '),
        b'H' => push_padded(out, hour(tm, specifier)? as u64, 2, b'0'),
        // C99 %I: "the hour (12-hour clock) as a decimal number (01-12)". Midnight and noon are
        // 12, not 0: the remainder mod 12 is 0 at both and the clock has no 0.
        b'I' => {
            let hour = hour(tm, specifier)?;
            let twelve = if hour % 12 == 0 { 12 } else { hour % 12 };
            push_padded(out, twelve as u64, 2, b'0');
        }
        // C99 %j: "the day of the year as a decimal number (001-366)". `tm_yday` is 0-based, so
        // the +1 is the conversion; omitting it is off by one for every day of every year and
        // still produces three plausible digits.
        b'j' => push_padded(out, (yday(tm, specifier)? + 1) as u64, 3, b'0'),
        // `tm_mon` is 0-based and %m is 1-based, same shape as %j.
        b'm' => push_padded(out, (mon(tm, specifier)? + 1) as u64, 2, b'0'),
        b'M' => push_padded(out, field(tm.min, "tm_min", 0, 59, specifier)? as u64, 2, b'0'),
        // 60 is allowed: POSIX bounds tm_sec at [0,60] so a positive leap second can be
        // represented, even though a value produced by `gmtime` never is one.
        b'S' => push_padded(out, field(tm.sec, "tm_sec", 0, 60, specifier)? as u64, 2, b'0'),
        b'n' => out.push(b'\n'),
        b't' => out.push(b'\t'),
        // POSIX C locale am_pm: "AM"/"PM". Noon is PM and midnight is AM, which is what
        // `hour < 12` says; the off-by-one alternative (`hour <= 12`) is wrong for one hour a
        // day and right for the other twenty-three.
        b'p' => out.extend_from_slice(if hour(tm, specifier)? < 12 { b"AM" } else { b"PM" }),
        // C99 %u: "the ISO 8601 weekday as a decimal number (1-7), where Monday is 1". `tm_wday`
        // has Sunday as 0, so Sunday is 7 and every other day is itself.
        b'u' => {
            let wday = wday(tm, specifier)?;
            push_padded(out, if wday == 0 { 7 } else { wday as u64 }, 1, b'0');
        }
        // C99 %w: "the weekday as a decimal number (0-6), where Sunday is 0" — `tm_wday` exactly.
        b'w' => push_padded(out, wday(tm, specifier)? as u64, 1, b'0'),
        // C99 %U: "the week number of the year (the first Sunday as the first day of week 1) as
        // a decimal number (00-53)".
        //
        // Derivation: `tm_yday - tm_wday` is the day of year of the Sunday that begins this
        // week, as a 0-based number that is negative for the days before the year's first
        // Sunday. Adding 7 and dividing by 7 maps that Sunday to 1, the one a week earlier to 2,
        // and every day before the first Sunday to 0 — which is what "week 0" means. The
        // numerator is in [1, 372] for every in-range field pair, so the truncating `/` is the
        // floor and no sign question arises.
        b'U' => {
            let (yday, wday) = (yday(tm, specifier)?, wday(tm, specifier)?);
            push_padded(out, ((yday + 7 - wday) / 7) as u64, 2, b'0');
        }
        // C99 %W: the same with Monday as the first day of the week. The only change is the
        // weekday index: `(tm_wday + 6) % 7` re-bases Sunday-first onto Monday-first.
        b'W' => {
            let (yday, wday) = (yday(tm, specifier)?, wday(tm, specifier)?);
            push_padded(out, ((yday + 7 - (wday + 6) % 7) / 7) as u64, 2, b'0');
        }
        // C99 %V: "the ISO 8601 week number as a decimal number (01-53)". NOT %U and NOT %W: see
        // `iso_week_date`.
        b'V' => {
            let (_, week) = iso_week_date(tm, specifier)?;
            push_padded(out, week as u64, 2, b'0');
        }
        b'G' => {
            let (year, _) = iso_week_date(tm, specifier)?;
            let year = year_representable(year, specifier, 1000, 9999)?;
            push_padded(out, year as u64, 4, b'0');
        }
        b'g' => {
            let (year, _) = iso_week_date(tm, specifier)?;
            let year = year_representable(year, specifier, 0, 9999)?;
            push_padded(out, (year % 100) as u64, 2, b'0');
        }
        b'y' => {
            let year = year_within(tm, specifier, 0, 9999)?;
            push_padded(out, (year % 100) as u64, 2, b'0');
        }
        b'Y' => {
            let year = year_within(tm, specifier, 1000, 9999)?;
            push_padded(out, year as u64, 4, b'0');
        }

        // ---- the two bionic extension fields ----------------------------------------------
        // POSIX %z: "the offset from UTC in the ISO 8601:2000 standard format (+hhmm or -hhmm)".
        // Four digits and no colon; `+hh:mm` is the GNU `%:z`, a different conversion. Seconds
        // are dropped rather than rounded — the format has no room for them, and rounding would
        // move a historical LMT offset to a minute it never had.
        b'z' => {
            let offset = when.gmtoff;
            if !(-86_400..=86_400).contains(&offset) {
                return Err(StrftimeError::GmtoffOutOfRange { gmtoff: offset });
            }
            // `+` for zero: UTC is +0000, and C has no negative zero here.
            out.push(if offset < 0 { b'-' } else { b'+' });
            // `unsigned_abs`, not `-offset`: negating `i64::MIN` overflows, and this value is a
            // `long` the guest chose. The range check above already refuses it, so this is belt
            // and braces — but the check and the negation are five lines apart and only one of
            // them is obviously load-bearing.
            let magnitude = offset.unsigned_abs();
            push_padded(out, magnitude / 3600, 2, b'0');
            push_padded(out, (magnitude / 60) % 60, 2, b'0');
        }
        b'Z' => match when.zone {
            Some(zone) => out.extend_from_slice(zone),
            None => return Err(StrftimeError::TimeZoneNameUnavailable),
        },

        b'%' => out.push(b'%'),

        // ---- the refusals ------------------------------------------------------------------
        b's' => return Err(StrftimeError::SecondsSinceEpochUnavailable),
        b'k' | b'l' | b'P' | b'v' | b'+' => {
            let what = match specifier {
                b'k' => "GNU/BSD: the 24-hour hour, blank-padded",
                b'l' => "GNU/BSD: the 12-hour hour, blank-padded",
                b'P' => "GNU: am/pm in lower case",
                b'v' => "BSD: %e-%b-%Y",
                _ => "BSD: date(1)'s default format",
            };
            return Err(StrftimeError::ExtensionSpecifier { specifier, what });
        }
        _ => return Err(StrftimeError::UnknownSpecifier { specifier }),
    }
    Ok(())
}

/// Check one `struct tm` field against the range POSIX `<time.h>` fixes for it, widening to
/// `i64` so that every arithmetic expression above is over a type the field cannot overflow.
fn field(
    value: i32,
    name: &'static str,
    lower: i32,
    upper: i32,
    specifier: u8,
) -> Result<i64, StrftimeError> {
    if value < lower || value > upper {
        return Err(StrftimeError::FieldOutOfRange {
            specifier,
            field: name,
            value,
            lower,
            upper,
        });
    }
    Ok(i64::from(value))
}

/// `tm_wday`, checked. POSIX: `[0, 6]`, Sunday is 0.
fn wday(tm: &Tm, specifier: u8) -> Result<i64, StrftimeError> {
    field(tm.wday, "tm_wday", 0, 6, specifier)
}

/// `tm_mon`, checked. POSIX: `[0, 11]`, January is 0.
fn mon(tm: &Tm, specifier: u8) -> Result<i64, StrftimeError> {
    field(tm.mon, "tm_mon", 0, 11, specifier)
}

/// `tm_mday`, checked. POSIX: `[1, 31]` — 1-based, unlike every other date field.
fn mday(tm: &Tm, specifier: u8) -> Result<i64, StrftimeError> {
    field(tm.mday, "tm_mday", 1, 31, specifier)
}

/// `tm_hour`, checked. POSIX: `[0, 23]`.
fn hour(tm: &Tm, specifier: u8) -> Result<i64, StrftimeError> {
    field(tm.hour, "tm_hour", 0, 23, specifier)
}

/// `tm_yday`, checked. POSIX: `[0, 365]` — 0-based, so 365 is the 366th day of a leap year.
fn yday(tm: &Tm, specifier: u8) -> Result<i64, StrftimeError> {
    field(tm.yday, "tm_yday", 0, 365, specifier)
}

/// The proleptic Gregorian year the `tm` names.
///
/// `i64::from` before the addition, not after: `tm_year + 1900` in `i32` overflows for
/// `tm_year > i32::MAX - 1900`, which a guest can set, and in a release build it wraps to a
/// negative year rather than panicking (`docs/VERIFICATION.md` entry 3). Widened first, the
/// addition cannot overflow at all — the sum of an `i32` and 1900 is inside `i64` for every
/// `i32` — which is why this is not a `checked_add`: there is nothing to check.
fn calendar_year(tm: &Tm) -> i64 {
    i64::from(tm.year) + 1900
}

/// [`calendar_year`], refused when it is outside the range the specifier can represent.
fn year_within(
    tm: &Tm,
    specifier: u8,
    lower: i64,
    upper: i64,
) -> Result<i64, StrftimeError> {
    year_representable(calendar_year(tm), specifier, lower, upper)
}

/// The same check for a year that has already been computed (`%G` and `%g`'s ISO year).
fn year_representable(
    year: i64,
    specifier: u8,
    lower: i64,
    upper: i64,
) -> Result<i64, StrftimeError> {
    if year < lower || year > upper {
        return Err(StrftimeError::YearNotRepresentable { specifier, year, lower, upper });
    }
    Ok(year)
}

/// The ISO 8601 week-based year and week number: `(%G, %V)`.
///
/// # The derivation, written out, because this is the specifier set that is easy to get subtly
/// wrong
///
/// ISO 8601 numbers weeks from **Monday** and puts week 1 as the week containing the year's
/// first Thursday — equivalently, the week containing 4 January, equivalently the week holding
/// the majority of its days in the new year. That has two consequences `%U` and `%W` do not
/// have: the first days of January can belong to **the previous** ISO year, and the last days of
/// December can belong to **the next** one. `%U` and `%W` never do either; they have a week 0
/// instead, and they always stay inside their own calendar year. An implementation that computes
/// `%V` as `%W + 1` agrees with ISO for most of the year, which is what makes it dangerous.
///
/// Step 1, the provisional week. With `iso_wday = (tm_wday + 6) % 7` (Monday 0 … Sunday 6),
/// `week = (tm_yday - iso_wday + 10) / 7`. The `+10` is `+7` for "count weeks from one, not
/// zero" and `+3` for "the week is numbered by the year its Thursday falls in" — `tm_yday -
/// iso_wday + 3` is the day of year of this week's Thursday. Over the POSIX field ranges the
/// numerator is in `[4, 375]`, so the truncating `/` is a floor and `week` is in `[0, 53]`.
///
/// Step 2, `week == 0`: this week's Thursday fell in the previous year, so the date belongs to
/// the previous ISO year's **last** week, which is 52 or 53.
///
/// Step 3, `week` beyond the year's own count: this week's Thursday fell in the next year, so
/// the date is week 1 of the next ISO year. Only a provisional 53 can be beyond it.
///
/// A year has 53 ISO weeks exactly when 1 January is a Thursday, or it is a leap year and 1
/// January is a Wednesday — in both cases the extra day (or two) pushes a 53rd Thursday into the
/// year. 1 January's weekday comes from the `tm` itself: `(tm_wday - tm_yday) mod 7` walks back
/// to day 0 of the same year. `rem_euclid`, not `%`, because that difference is negative for
/// every day after 1 January and `%` would give a negative index.
///
/// The previous year's 1 January is `(this year's - 365 - leap(previous)) mod 7`, i.e. one day
/// earlier for a common year and two for a leap one.
///
/// # What this deliberately does not do
///
/// It does not re-derive `tm_yday` or `tm_wday` from the date: it uses the fields it is given,
/// which is what every other conversion here does and what the standard's own formulation is in
/// terms of. A `tm` whose fields disagree with each other produces a defined answer that reflects
/// the fields it was handed.
fn iso_week_date(tm: &Tm, specifier: u8) -> Result<(i64, i64), StrftimeError> {
    let yday = yday(tm, specifier)?;
    let wday = wday(tm, specifier)?;
    // Widened from `i32` and never larger than about 2.1e9, so every sum below is inside `i64`
    // by many orders of magnitude — the fields it is combined with are bounded by 365.
    let year = calendar_year(tm);

    let iso_wday = (wday + 6) % 7;
    let week = (yday - iso_wday + 10) / 7;
    let january_first = (wday - yday).rem_euclid(7);

    if week == 0 {
        let previous = year - 1;
        let previous_first = (january_first - 1 - i64::from(is_leap(previous))).rem_euclid(7);
        return Ok((previous, weeks_in_iso_year(previous, previous_first)));
    }
    if week > weeks_in_iso_year(year, january_first) {
        return Ok((year + 1, 1));
    }
    Ok((year, week))
}

/// How many ISO 8601 weeks `year` has, given the weekday of its 1 January (Sunday 0).
///
/// 52 or 53, never anything else: 52 weeks is 364 days and a year is 365 or 366, so at most one
/// extra week can form. See [`iso_week_date`] for why the condition is "Thursday, or Wednesday in
/// a leap year".
fn weeks_in_iso_year(year: i64, january_first: i64) -> i64 {
    /// Thursday, in `tm_wday`'s Sunday-is-0 numbering.
    const THURSDAY: i64 = 4;
    /// Wednesday, likewise.
    const WEDNESDAY: i64 = 3;
    if january_first == THURSDAY || (is_leap(year) && january_first == WEDNESDAY) {
        53
    } else {
        52
    }
}

/// Append `value` in decimal, padded on the left to `width` with `pad`.
///
/// `u64` rather than a signed type on purpose. Every conversion that reaches here has already
/// refused a negative value — the fields are range-checked and the years are range-checked — so a
/// sign branch would be a branch no input can take (`docs/VERIFICATION.md` entry 12). `%z`'s sign
/// is pushed by `%z`, which is the only conversion that has one.
///
/// A value wider than `width` is **not** truncated: it is printed in full. C's `%02d` does the
/// same, and truncating a year to its last two digits because the field said two is how a
/// `strftime` prints 2026 as `26`.
fn push_padded(out: &mut Vec<u8>, value: u64, width: usize, pad: u8) {
    // 20 digits is `u64::MAX`, so the buffer cannot be overrun by any input.
    let mut digits = [0u8; 20];
    let mut count = 0usize;
    let mut rest = value;
    loop {
        digits[count] = b'0' + (rest % 10) as u8;
        rest /= 10;
        count += 1;
        if rest == 0 {
            break;
        }
    }
    for _ in count..width {
        out.push(pad);
    }
    for &digit in digits[..count].iter().rev() {
        out.push(digit);
    }
}

/// A guest `struct tm`, read back out of guest memory.
///
/// The companion to [`write_tm`], and the shape `strftime` needs: unlike `gmtime`, `strftime`
/// takes a `struct tm` as **input**, including the two BSD extension fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestTm {
    /// The nine POSIX fields, unvalidated — see [`StrftimeTm::tm`].
    pub tm: Tm,
    /// `tm_gmtoff`, the guest's `long`.
    pub gmtoff: i64,
    /// `tm_zone`, the guest **pointer**. Resolving it to bytes needs a string read the caller
    /// owns, so it is left as an address here; `0` is NULL.
    pub zone: u64,
}

/// Read the 56 bytes of a guest `struct tm`.
///
/// One [`GuestMemory::read`] of the whole structure rather than eleven field reads, for the
/// mirror of the reason [`write_tm`] does one write: a `struct tm` that is half-read across a
/// mapping boundary would be a *partly* stale broken-down time, and every field of it looks like
/// a time.
///
/// The fields are returned exactly as they were found. No range checking happens here — the
/// ranges belong to the conversions that read the fields, and rejecting a whole `struct tm`
/// because `tm_isdst` is 7 would refuse calls bionic answers.
///
/// # Errors
///
/// [`Fault`] if the 56 bytes at `at` are not readable guest memory.
pub fn read_tm(mem: &impl GuestMemory, at: u64) -> Result<GuestTm, Fault> {
    checked_range(at, TM_BYTES as u64)?;
    let mut bytes = [0u8; TM_BYTES];
    mem.read(at, &mut bytes)?;
    let int_at = |offset: usize| {
        // The slice is 4 bytes of a 56-byte array at a fixed offset, so the conversion cannot
        // fail; `unwrap_or` keeps it total without a panic path rather than asserting it.
        i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap_or([0; 4]))
    };
    Ok(GuestTm {
        tm: Tm {
            sec: int_at(0),
            min: int_at(4),
            hour: int_at(8),
            mday: int_at(12),
            mon: int_at(16),
            year: int_at(20),
            wday: int_at(24),
            yday: int_at(28),
            isdst: int_at(32),
        },
        gmtoff: i64::from_le_bytes(
            bytes[TM_GMTOFF_OFFSET..TM_GMTOFF_OFFSET + 8].try_into().unwrap_or([0; 8]),
        ),
        zone: u64::from_le_bytes(
            bytes[TM_ZONE_OFFSET..TM_ZONE_OFFSET + 8].try_into().unwrap_or([0; 8]),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockMemory;

    /// **`mktime` is `gmtime`'s inverse over every timestamp `gmtime` accepts**, asserted as a
    /// round trip rather than against a second table.
    ///
    /// VERIFICATION entry 7 is why this is a round trip and not a comparison with `chrono` or
    /// with the host's own `mktime`: agreeing with a second implementation proves only that both
    /// did the same thing. The property under test is the one C states -- `mktime` and `gmtime`
    /// are inverses on normalised input -- and it is checked on the dates whose weekday the
    /// suite above already pins independently, plus the two ends of the representable range.
    #[test]
    fn mktime_is_the_exact_inverse_of_gmtime() {
        for timestamp in [
            0i64,
            -1,
            951_782_400,      // 2000-02-29, a leap day in a leap century
            4_107_542_399,    // 2100-02-28T23:59:59, the century that is NOT a leap year
            1_774_137_600,    // 2026-03-22
            -2_208_988_800,   // 1900-01-01, before the epoch
            67_768_036_191_676_799, // the last second `struct tm`'s int tm_year can hold
            -67_768_040_609_740_800,
        ] {
            let tm = gmtime(timestamp).expect("in range for tm_year");
            let (back, normalised) =
                mktime(&tm).unwrap_or_else(|e| panic!("mktime({timestamp}): {e}"));
            assert_eq!(back, timestamp, "round trip of {timestamp} through {tm:?}");
            assert_eq!(normalised, tm, "already-normalised input must come back unchanged");
        }
    }

    /// **Out-of-range fields normalise**, which is `mktime`'s whole reason for existing.
    ///
    /// Each row states the denormalised fields and the date they mean, derived from C 7.29.2.3
    /// ("the values ... are not restricted to the ranges indicated") rather than from another
    /// implementation's output. The 61st second is the row that matters most: a `struct tm` built
    /// from an X.509 time string is exactly this shape when the string is malformed, and an
    /// implementation that clamped rather than carried would accept a certificate for the wrong
    /// second and never say so.
    #[test]
    fn denormalised_fields_carry_rather_than_clamp() {
        /// `(sec, min, hour, mday, mon, year)` and the normalised `(year, mon, mday, hour, min, sec)`.
        struct Row {
            given: (i32, i32, i32, i32, i32, i32),
            means: (i32, i32, i32, i32, i32, i32),
        }
        for row in [
            // 2026-01-01T00:00:60 is 2026-01-01T00:01:00.
            Row { given: (60, 0, 0, 1, 0, 126), means: (126, 0, 1, 0, 1, 0) },
            // Month 12 of 2025 is January 2026.
            Row { given: (0, 0, 0, 1, 12, 125), means: (126, 0, 1, 0, 0, 0) },
            // Month -1 of 2026 is December 2025.
            Row { given: (0, 0, 0, 1, -1, 126), means: (125, 11, 1, 0, 0, 0) },
            // Day 0 of March is the last day of February -- and 2026 is not a leap year.
            Row { given: (0, 0, 0, 0, 2, 126), means: (126, 1, 28, 0, 0, 0) },
            // Day 0 of March 2024 is the 29th, because 2024 is.
            Row { given: (0, 0, 0, 0, 2, 124), means: (124, 1, 29, 0, 0, 0) },
            // 25 hours past midnight is the next day at 01:00.
            Row { given: (0, 0, 25, 1, 0, 126), means: (126, 0, 2, 1, 0, 0) },
            // A negative second borrows from the day before.
            Row { given: (-1, 0, 0, 1, 0, 126), means: (125, 11, 31, 23, 59, 59) },
        ] {
            let (sec, min, hour, mday, mon, year) = row.given;
            let tm = Tm { sec, min, hour, mday, mon, year, wday: 99, yday: 99, isdst: -1 };
            let (_, got) = mktime(&tm).expect("a representable date");
            assert_eq!(
                (got.year, got.mon, got.mday, got.hour, got.min, got.sec),
                row.means,
                "normalising {:?}",
                row.given
            );
        }
    }

    /// **`tm_wday` and `tm_yday` in the input are ignored and replaced**, as C requires.
    ///
    /// Asserted with deliberately wrong values in, because an implementation that *used* them
    /// would agree with a correct one on every input where they happen to be right -- which is
    /// every input a test writer would naturally construct.
    #[test]
    fn the_weekday_and_day_of_year_are_computed_rather_than_believed() {
        let tm = Tm { sec: 0, min: 0, hour: 0, mday: 1, mon: 0, year: 70, wday: 5, yday: 200, isdst: 0 };
        let (seconds, normalised) = mktime(&tm).expect("the epoch");
        assert_eq!(seconds, 0, "the wrong wday/yday must not move the timestamp");
        assert_eq!(normalised.wday, 4, "1970-01-01 was a Thursday");
        assert_eq!(normalised.yday, 0, "and the first day of the year");
    }

    /// **No `struct tm` can make this panic or wrap**, over every extreme of every field.
    ///
    /// VERIFICATION entry 3 in person: `mktime` multiplies guest-controlled `int`s by 86,400 and
    /// by 12, a release build wraps, and a wrapped timestamp is a date. This is run in whatever
    /// profile the suite runs in, and the `checked_*` chain is what makes the debug profile agree
    /// with it rather than panic.
    #[test]
    fn no_broken_down_time_at_all_can_make_this_panic_or_wrap() {
        let extremes = [i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX];
        for &year in &extremes {
            for &mon in &extremes {
                for &mday in &extremes {
                    for &hour in &[i32::MIN, 0, i32::MAX] {
                        let tm = Tm {
                            sec: i32::MAX,
                            min: i32::MIN,
                            hour,
                            mday,
                            mon,
                            year,
                            wday: 0,
                            yday: 0,
                            isdst: 0,
                        };
                        // Either a timestamp or a named refusal; never a panic and never a wrap.
                        if let Ok((seconds, normalised)) = mktime(&tm) {
                            assert_eq!(
                                gmtime(seconds).expect("mktime only returns representable dates"),
                                normalised,
                                "a returned pair must agree with gmtime: {tm:?}"
                            );
                        }
                    }
                }
            }
        }
    }

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
    /// **This is a sample, not an enumeration, and it is not the detector for the wrap.** An
    /// earlier version of this comment claimed otherwise; a review found the claim false and the
    /// test nearly empty, so both are written down here.
    ///
    /// Why the wrap cannot be caught from a return value: `days * SECONDS_PER_DAY` can only exceed
    /// `i64` near `i64::MIN`, where the floor pushes the product below the input by up to
    /// `SECONDS_PER_DAY - 1`. Every such timestamp falls in a year around -2.9e11, so
    /// `i32::try_from(year - 1900)` refuses it **whatever `second_of_day` holds**. In a release
    /// build the product wraps, the garbage is discarded by that refusal, and the observable
    /// behaviour is correct by accident. So no assertion here can see it.
    ///
    /// The detector is the **debug** build's panic on the multiplication, and the thing that runs
    /// this in debug is `tools/mutate.py`. Row `time-A7` restores the unchecked expression for
    /// exactly that reason; without the row, nothing at all detects a regression.
    ///
    /// What this test does assert is the total function: every `i64` returns, none panics, an `Ok`
    /// carries fields inside their ranges, and an `Err` carries a year that genuinely does not fit
    /// — that last one so the refusal arm cannot be satisfied by refusing everything.
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
                // NOT an empty arm. Six of the ten edges land here, and an empty arm would let
                // this whole test pass against an implementation that refused every input.
                Err(GmtimeError::YearOutOfRange { year }) => {
                    assert!(
                        i32::try_from(year - 1900).is_err(),
                        "{timestamp}: refused with year {year}, which does fit `int tm_year` —                          a refusal is only correct when the year is genuinely out of range",
                    );
                }
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
