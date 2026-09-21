//! `strftime`: every conversion, every refusal, and the return-value rule.
//!
//! # Where the expected strings come from
//!
//! **From the specification, never from another `strftime`.** `docs/VERIFICATION.md` entry 7 is
//! the reason: `inet_ntop` was checked against Rust's `Ipv6Addr: Display`, which looks equivalent
//! to bionic's and disagreed with it in 43 of 200,000 cases. Nothing here calls `chrono`, `time`,
//! the host's `strftime` or any other implementation, and no expected string below was copied
//! from one.
//!
//! The sources actually used, each named at the assertion that leans on it:
//!
//! * **C99 7.23.3.5** for the conversion list, the `%E`/`%O` rule and the return value;
//! * **POSIX `strftime`** and the **POSIX C locale** definition (`abday`, `day`, `abmon`, `mon`,
//!   `am_pm`, `d_fmt`, `t_fmt`, `d_t_fmt`, `t_fmt_ampm`) for the names and compound formats;
//! * **ISO 8601** for `%V`, `%G` and `%g`;
//! * **POSIX `<time.h>`** for each `struct tm` field's range.
//!
//! # Where the dates come from
//!
//! The four reference `tm`s below are calendar facts, each cross-checked two ways: the day of
//! the year is re-counted from the month lengths, and the weekday is stepped from a weekday
//! already pinned by `time.rs`'s own table (1970-01-01 is a Thursday; 2026-09-10 is a Thursday).
//! The arithmetic is written out beside each one so a reader can redo it without an almanac.
//!
//! # What is asserted
//!
//! Membership, not totals (`docs/VERIFICATION.md` entry 1). Every conversion is named in a row
//! of its own with its own expected bytes; there is no assertion anywhere here of the form
//! "n conversions work".

use omni_bionic::mock::MockMemory;
use omni_bionic::time::{
    gmtime, read_tm, strftime, write_tm, StrftimeError, StrftimeOutput, StrftimeTm, Tm,
    MAX_STRFTIME_OUTPUT,
};

/// 2026-09-21T14:05:09Z.
///
/// Day of year: January-August in a non-leap year is 31+28+31+30+31+30+31+31 = 243 days, so
/// 1 September is `tm_yday` 243 and 21 September is 263.
///
/// Weekday: `time.rs`'s table pins 2026-09-10 as a Thursday (`tm_wday` 4) at `tm_yday` 252.
/// 263 - 252 = 11 days later, and (4 + 11) mod 7 = 1 — a **Monday**.
const MONDAY_AFTERNOON: Tm = Tm {
    sec: 9,
    min: 5,
    hour: 14,
    mday: 21,
    mon: 8,
    year: 126,
    wday: 1,
    yday: 263,
    isdst: 0,
};

/// 1970-01-01T00:00:00Z, the epoch, a Thursday — pinned by `time.rs`'s first table row.
///
/// Kept as a reference because it is the one date whose `tm` this suite can obtain a second way,
/// from [`gmtime`] of zero, and because a single-digit day of the month is what tells `%e` from
/// `%d`.
const EPOCH: Tm =
    Tm { sec: 0, min: 0, hour: 0, mday: 1, mon: 0, year: 70, wday: 4, yday: 0, isdst: 0 };

/// 2024-02-29T00:00:00Z, a leap day.
///
/// Day of year: 31 days of January puts 1 February at 31, and 29 February is 31 + 28 = 59.
///
/// Weekday: 1 January 2024 is a Monday (`tm_wday` 1) — 2023 is a common year of 365 days and
/// 1 January 2023 was a Sunday, and 365 mod 7 = 1 advances Sunday to Monday. From there
/// (1 + 59) mod 7 = 4, a **Thursday**.
const LEAP_DAY: Tm =
    Tm { sec: 0, min: 0, hour: 0, mday: 29, mon: 1, year: 124, wday: 4, yday: 59, isdst: 0 };

/// 2026-01-01T00:00:00Z, a Thursday — `time.rs`'s own table computes this date from timestamp
/// 1,767,225,600 and the weekday follows from 2026-09-10 being a Thursday at `tm_yday` 252:
/// (4 - 252) mod 7 = 4.
const NEW_YEARS_DAY: Tm =
    Tm { sec: 0, min: 0, hour: 0, mday: 1, mon: 0, year: 126, wday: 4, yday: 0, isdst: 0 };

/// A `tm` in UTC with a zone name, which is what every `tm` this project produces looks like.
fn utc(tm: Tm) -> StrftimeTm<'static> {
    StrftimeTm { tm, gmtoff: 0, zone: Some(b"UTC") }
}

/// Format into a `String`, asserting the result fits. 4096 is far above anything these formats
/// produce, so the fit is never what is under test here — the boundary has its own test.
#[track_caller]
fn rendered(format: &[u8], when: &StrftimeTm<'_>) -> String {
    match strftime(4096, format, when) {
        Ok(StrftimeOutput::Fits(bytes)) => {
            String::from_utf8(bytes).expect("the C locale emits ASCII")
        }
        other => panic!("{:?} on format {:?}", other, String::from_utf8_lossy(format)),
    }
}

/// The error a format produces, asserting it produced one.
#[track_caller]
fn refused(format: &[u8], when: &StrftimeTm<'_>) -> StrftimeError {
    match strftime(4096, format, when) {
        Err(error) => error,
        Ok(output) => panic!(
            "format {:?} produced {:?} where a named refusal was required",
            String::from_utf8_lossy(format),
            output
        ),
    }
}

/// Every conversion this implementation supports, named one by one, on one known instant.
///
/// 2026-09-21T14:05:09Z, a Monday. Each row carries the clause it comes from. A row per
/// conversion rather than one long format string on purpose: a single string would fail as a
/// whole and say nothing about which conversion moved.
#[test]
fn every_implemented_conversion_on_a_known_monday_afternoon() {
    let when = utc(MONDAY_AFTERNOON);
    let rows: &[(&[u8], &str, &str)] = &[
        (b"%a", "Mon", "POSIX C locale abday, Sunday first because tm_wday counts Sunday as 0"),
        (b"%A", "Monday", "POSIX C locale day"),
        (b"%b", "Sep", "POSIX C locale abmon, tm_mon 8 is September"),
        (b"%B", "September", "POSIX C locale mon"),
        (b"%h", "Sep", "POSIX: %h is equivalent to %b"),
        (b"%c", "Mon Sep 21 14:05:09 2026", "POSIX C locale d_t_fmt '%a %b %e %H:%M:%S %Y'"),
        (b"%C", "20", "C99: the year divided by 100, truncated: 2026/100 = 20"),
        (b"%d", "21", "C99: day of the month, 01-31, zero-padded"),
        (b"%D", "09/21/26", "C99: equivalent to %m/%d/%y"),
        (b"%e", "21", "C99: like %d but space-padded; two digits here, so no padding shows"),
        (b"%F", "2026-09-21", "C99: equivalent to %Y-%m-%d"),
        (b"%g", "26", "ISO 8601 week-based year 2026, last two digits"),
        (b"%G", "2026", "ISO 8601 week-based year; see the ISO test for why it is not always %Y"),
        (b"%H", "14", "C99: the hour on a 24-hour clock, 00-23"),
        (b"%I", "02", "C99: the hour on a 12-hour clock, 01-12: 14 mod 12 = 2"),
        (b"%j", "264", "C99: day of the year 001-366, and tm_yday 263 is 0-based"),
        (b"%m", "09", "C99: the month 01-12, and tm_mon 8 is 0-based"),
        (b"%M", "05", "C99: the minute 00-59"),
        (b"%n", "\n", "C99: a newline"),
        (b"%p", "PM", "POSIX C locale am_pm; 14 is not before noon"),
        (b"%r", "02:05:09 PM", "POSIX C locale t_fmt_ampm '%I:%M:%S %p'"),
        (b"%R", "14:05", "C99: equivalent to %H:%M"),
        (b"%S", "09", "C99: the second 00-60"),
        (b"%t", "\t", "C99: a tab"),
        (b"%T", "14:05:09", "C99: equivalent to %H:%M:%S"),
        (b"%u", "1", "C99: ISO weekday 1-7, Monday is 1 — and this is a Monday"),
        (b"%U", "38", "C99 %U: (263 + 7 - 1)/7 = 38; week 1 began on Sunday 4 January"),
        (b"%V", "39", "ISO 8601: (263 - 0 + 10)/7 = 39; ISO week 1 of 2026 began Mon 29 Dec 2025"),
        (b"%w", "1", "C99: weekday 0-6, Sunday is 0 — tm_wday verbatim"),
        (b"%W", "38", "C99 %W: (263 + 7 - 0)/7 = 38; week 1 began on Monday 5 January"),
        (b"%x", "09/21/26", "POSIX C locale d_fmt '%m/%d/%y'"),
        (b"%X", "14:05:09", "POSIX C locale t_fmt '%H:%M:%S'"),
        (b"%y", "26", "C99: the last two digits of the year"),
        (b"%Y", "2026", "C99: the year as a decimal number"),
        (b"%z", "+0000", "POSIX: +hhmm; tm_gmtoff is 0 and UTC is +0000, never -0000"),
        (b"%Z", "UTC", "POSIX: the zone name, which is tm_zone's bytes"),
        (b"%%", "%", "C99: a literal %"),
    ];
    for (format, expected, source) in rows {
        assert_eq!(
            rendered(format, &when),
            *expected,
            "{} — {source}",
            String::from_utf8_lossy(format)
        );
    }
}

/// The epoch, where the day of the month is a single digit — which is the whole of `%e`.
///
/// `%c`'s two spaces before the `1` are not a typo: `%c` is `%a %b %e ...`, `%e` is space-padded,
/// and the space-padded `1` is ` 1`. A `%c` built on `%d` would print `Thu Jan 01 ...`, which is
/// a believable wrong answer and the reason this instant is here.
#[test]
fn the_epoch_shows_the_space_padding_that_tells_percent_e_from_percent_d() {
    let when = utc(EPOCH);
    assert_eq!(rendered(b"%e", &when), " 1", "C99: a single digit is preceded by a space");
    assert_eq!(rendered(b"%d", &when), "01", "C99: %d zero-pads where %e space-pads");
    assert_eq!(rendered(b"%j", &when), "001", "C99: three digits, and tm_yday 0 is day 1");
    assert_eq!(rendered(b"%c", &when), "Thu Jan  1 00:00:00 1970");
    assert_eq!(rendered(b"%I", &when), "12", "C99: midnight is 12 on a 12-hour clock, not 00");
    assert_eq!(rendered(b"%p", &when), "AM", "midnight is before noon");
    assert_eq!(rendered(b"%r", &when), "12:00:00 AM");
    assert_eq!(rendered(b"%C%y", &when), "1970", "C99: %C is 19 and %y is 70");
    assert_eq!(rendered(b"%U", &when), "00", "no Sunday has happened yet in 1970");
    assert_eq!(rendered(b"%W", &when), "00", "no Monday has happened yet in 1970");
    assert_eq!(
        rendered(b"%G-W%V-%u", &when),
        "1970-W01-4",
        "ISO 8601: 1 January 1970 is a Thursday, so it is in week 1 by definition"
    );
}

/// Noon is PM and 12 on a 12-hour clock: the other end of the `%I`/`%p` boundary.
///
/// The believable wrong answers are `00` for `%I` (the remainder without the special case) and
/// `AM` for `%p` (`hour <= 12`). Each is wrong for exactly one hour a day.
#[test]
fn noon_and_the_hour_before_it_are_the_boundary_of_percent_i_and_percent_p() {
    let mut before = MONDAY_AFTERNOON;
    before.hour = 11;
    assert_eq!(rendered(b"%I %p", &utc(before)), "11 AM");
    let mut noon = MONDAY_AFTERNOON;
    noon.hour = 12;
    assert_eq!(rendered(b"%I %p", &utc(noon)), "12 PM", "noon is 12 PM, not 00 PM or 12 AM");
    let mut midnight = MONDAY_AFTERNOON;
    midnight.hour = 0;
    assert_eq!(rendered(b"%I %p", &utc(midnight)), "12 AM");
    let mut last = MONDAY_AFTERNOON;
    last.hour = 23;
    assert_eq!(rendered(b"%I %p", &utc(last)), "11 PM");
}

/// A leap day, which is where a day-of-year table is wrong if it is wrong.
#[test]
fn the_leap_day_formats_as_the_twenty_ninth_of_february() {
    let when = utc(LEAP_DAY);
    assert_eq!(rendered(b"%F", &when), "2024-02-29");
    assert_eq!(rendered(b"%A %e %B %Y", &when), "Thursday 29 February 2024");
    assert_eq!(rendered(b"%j", &when), "060", "31 days of January + 29 = the 60th day");
    assert_eq!(rendered(b"%U", &when), "08", "(59 + 7 - 4)/7 = 8");
    assert_eq!(rendered(b"%W", &when), "09", "(59 + 7 - 3)/7 = 9, and 1 January 2024 is a Monday");
    assert_eq!(rendered(b"%V", &when), "09", "(59 - 3 + 10)/7 = 9");
}

/// ISO 8601 week numbering is **not** `%U` and **not** `%W`, and these five dates are where it
/// visibly is not.
///
/// Each row is derived from the ISO rule — weeks run Monday to Sunday, and week 1 is the week
/// containing the year's first Thursday, equivalently 4 January — and the derivation is in the
/// row. The believable wrong answer this catches is `%V` computed as `%W` or as `%W + 1`: those
/// agree with ISO for most of the year and disagree exactly here.
#[test]
fn iso_week_numbering_is_not_the_same_as_percent_u_or_percent_w() {
    struct Row {
        tm: Tm,
        iso_year: &'static str,
        iso_week: &'static str,
        sunday_week: &'static str,
        monday_week: &'static str,
        why: &'static str,
    }
    let rows = &[
        Row {
            // 2027-01-01, a Friday: 2026 is a common year, and 1 January 2026 is a Thursday, so
            // 1 January 2027 is a Friday.
            tm: Tm { sec: 0, min: 0, hour: 0, mday: 1, mon: 0, year: 127, wday: 5, yday: 0, isdst: 0 },
            iso_year: "2026",
            iso_week: "53",
            sunday_week: "00",
            monday_week: "00",
            why: "the first Thursday of 2027 is the 7th, so 1 January belongs to 2026's last \
                  week — and 2026 has 53 of them because its 1 January is a Thursday",
        },
        Row {
            // 2024-12-30, a Monday: 2024 is a leap year, so 30 December is tm_yday 364.
            // 1 January 2024 is a Monday and 364 mod 7 = 0, so 30 December is one too.
            tm: Tm { sec: 0, min: 0, hour: 0, mday: 30, mon: 11, year: 124, wday: 1, yday: 364, isdst: 0 },
            iso_year: "2025",
            iso_week: "01",
            sunday_week: "52",
            monday_week: "53",
            why: "this week's Thursday is 2 January 2025, so the week is week 1 of 2025 — while \
                  %W, which cannot leave its own calendar year, calls it week 53 of 2024",
        },
        Row {
            // 2020-12-31, a Thursday: 2020 is a leap year, so 31 December is tm_yday 365.
            // 1 January 2020 is a Wednesday and 365 mod 7 = 1, so 31 December is a Thursday.
            tm: Tm { sec: 0, min: 0, hour: 0, mday: 31, mon: 11, year: 120, wday: 4, yday: 365, isdst: 0 },
            iso_year: "2020",
            iso_week: "53",
            sunday_week: "52",
            monday_week: "52",
            why: "2020 is a leap year whose 1 January is a Wednesday, so it has 53 ISO weeks and \
                  its own last day is in the 53rd",
        },
        Row {
            // 2021-01-04, a Monday: the day after 2020-12-31, a Thursday, plus four days.
            tm: Tm { sec: 0, min: 0, hour: 0, mday: 4, mon: 0, year: 121, wday: 1, yday: 3, isdst: 0 },
            iso_year: "2021",
            iso_week: "01",
            sunday_week: "01",
            monday_week: "01",
            why: "4 January is in week 1 of its own year by ISO's definition, whatever weekday \
                  it falls on",
        },
        Row {
            // 2023-01-01, a Sunday: 2022 is a common year whose 1 January is a Saturday.
            tm: Tm { sec: 0, min: 0, hour: 0, mday: 1, mon: 0, year: 123, wday: 0, yday: 0, isdst: 0 },
            iso_year: "2022",
            iso_week: "52",
            sunday_week: "01",
            monday_week: "00",
            why: "the most visible disagreement in the set: %U says week 1 (1 January is itself \
                  the first Sunday), %W says week 0, and ISO says the 52nd week of 2022",
        },
    ];
    for row in rows {
        let when = utc(row.tm);
        assert_eq!(rendered(b"%G", &when), row.iso_year, "%G: {}", row.why);
        assert_eq!(rendered(b"%V", &when), row.iso_week, "%V: {}", row.why);
        assert_eq!(rendered(b"%U", &when), row.sunday_week, "%U: {}", row.why);
        assert_eq!(rendered(b"%W", &when), row.monday_week, "%W: {}", row.why);
        assert_eq!(
            rendered(b"%g", &when),
            &row.iso_year[2..],
            "%g is the last two digits of %G"
        );
    }
}

/// The ISO week-date invariants over 4,000 consecutive days, derived from the definition rather
/// than from a table.
///
/// n = 4,000 days from 2015-01-01, which spans two 53-week years (2015 and 2020) and six
/// year boundaries. Three properties, all straight from ISO 8601:
///
/// * a week runs Monday to Sunday, so `(%G, %V)` changes **only** on a Monday;
/// * within a week-based year the weeks are numbered consecutively, so a Monday either
///   increments `%V` or starts week 1 of the next week-based year;
/// * `%V` is in 01-53 and `%G` is within one year of `%Y`.
///
/// This is structural: it needs no almanac and cannot be satisfied by an implementation that
/// gets the *offset* right and the boundaries wrong.
#[test]
fn the_iso_week_date_advances_exactly_once_a_week_across_four_thousand_days() {
    let start = 1_420_070_400i64; // 2015-01-01T00:00:00Z
    let day = 86_400i64;
    let mut previous: Option<(i64, i64)> = None;
    for offset in 0..4_000i64 {
        let tm = gmtime(start + offset * day).expect("well inside tm_year's range");
        let when = utc(tm);
        let iso_year: i64 = rendered(b"%G", &when).parse().expect("four digits");
        let iso_week: i64 = rendered(b"%V", &when).parse().expect("two digits");
        assert!((1..=53).contains(&iso_week), "day {offset}: %V is {iso_week}");
        let calendar_year = i64::from(tm.year) + 1900;
        assert!(
            (calendar_year - 1..=calendar_year + 1).contains(&iso_year),
            "day {offset}: %G {iso_year} is more than a year from %Y {calendar_year}"
        );
        if let Some((previous_year, previous_week)) = previous {
            if tm.wday == 1 {
                let advanced = (iso_year, iso_week) == (previous_year, previous_week + 1);
                let rolled = (iso_year, iso_week) == (previous_year + 1, 1);
                assert!(
                    advanced || rolled,
                    "day {offset}: a Monday moved ({previous_year}, {previous_week}) to \
                     ({iso_year}, {iso_week}), which is neither the next week nor week 1 of the \
                     next week-based year"
                );
            } else {
                assert_eq!(
                    (iso_year, iso_week),
                    (previous_year, previous_week),
                    "day {offset}: the ISO week changed on a tm_wday of {}, and only Monday \
                     starts a week",
                    tm.wday
                );
            }
        }
        previous = Some((iso_year, iso_week));
    }
}

/// `%U` and `%W` advance on Sunday and on Monday respectively, and on no other day.
///
/// n = 365 days of 2026. Structural, for the same reason as the ISO test: it is the *definition*
/// of "the first Sunday as the first day of week 1" restated as something checkable.
#[test]
fn percent_u_advances_on_sundays_and_percent_w_on_mondays() {
    let start = 1_767_225_600i64; // 2026-01-01T00:00:00Z
    let day = 86_400i64;
    let mut previous: Option<(i64, i64)> = None;
    for offset in 0..365i64 {
        let tm = gmtime(start + offset * day).expect("in range");
        let when = utc(tm);
        let sunday_week: i64 = rendered(b"%U", &when).parse().expect("two digits");
        let monday_week: i64 = rendered(b"%W", &when).parse().expect("two digits");
        assert!((0..=53).contains(&sunday_week), "day {offset}: %U is {sunday_week}");
        assert!((0..=53).contains(&monday_week), "day {offset}: %W is {monday_week}");
        if let Some((previous_sunday, previous_monday)) = previous {
            let expected_sunday = previous_sunday + i64::from(tm.wday == 0);
            let expected_monday = previous_monday + i64::from(tm.wday == 1);
            assert_eq!(sunday_week, expected_sunday, "day {offset}: %U, tm_wday {}", tm.wday);
            assert_eq!(monday_week, expected_monday, "day {offset}: %W, tm_wday {}", tm.wday);
        }
        previous = Some((sunday_week, monday_week));
    }
    // 2026 begins on a Thursday, so the first Sunday is the 4th and the first Monday the 5th —
    // which means the year opens on week 0 under both and they are never equal to each other
    // until the first Monday. Pinning the opening value stops the loop above being satisfied by
    // an implementation that is consistently off by one.
    assert_eq!(rendered(b"%U %W", &utc(NEW_YEARS_DAY)), "00 00");
}

/// In the C locale `%E` and `%O` are no-ops, so `%E<c>` and `%O<c>` are `%<c>` for every
/// conversion this implementation handles — asserted **conversion by conversion**.
///
/// C99 7.23.3.5 paragraph 3: the modifiers request alternative representations, and "if the
/// alternative format or specification does not exist for the current locale, the modifier is
/// ignored". bionic has the C locale's behaviour and nothing else, and the C locale defines no
/// alternatives at all.
#[test]
fn the_e_and_o_modifiers_are_no_ops_in_the_c_locale_for_every_conversion() {
    let when = utc(MONDAY_AFTERNOON);
    let implemented = b"aAbBcCdDeFgGhHIjmMnprRStTuUVwWxXyYzZ%";
    for &conversion in implemented {
        let plain = rendered(&[b'%', conversion], &when);
        assert_eq!(
            rendered(&[b'%', b'E', conversion], &when),
            plain,
            "%E{} must be %{}",
            conversion as char,
            conversion as char
        );
        assert_eq!(
            rendered(&[b'%', b'O', conversion], &when),
            plain,
            "%O{} must be %{}",
            conversion as char,
            conversion as char
        );
    }
    // Exactly one modifier is consumed: the second one is then read as the conversion, and `O`
    // is not one. The alternative — looping over modifiers — would accept `%EEEEy`, which no
    // standard describes.
    assert_eq!(
        refused(b"%EOy", &when),
        StrftimeError::UnknownSpecifier { specifier: b'O' },
        "a doubled modifier is not a conversion"
    );
}

/// Every conversion this implementation refuses, named one by one with the error it gives.
///
/// This is the list `docs/HANDOFF.md` asks for: a conversion that is not implemented **refuses**,
/// rather than emitting literal text or a guess. The membership matters, not the count — a
/// version of this test that asserted "six conversions refuse" would pass with the wrong six.
#[test]
fn every_refused_conversion_is_refused_by_name() {
    let when = utc(MONDAY_AFTERNOON);
    assert_eq!(
        refused(b"%s", &when),
        StrftimeError::SecondsSinceEpochUnavailable,
        "%s is mktime(), which needs normalisation and a timezone database"
    );
    let extensions: &[(u8, &str)] = &[
        (b'k', "the 24-hour hour, blank-padded"),
        (b'l', "the 12-hour hour, blank-padded"),
        (b'P', "am/pm in lower case"),
        (b'v', "%e-%b-%Y"),
        (b'+', "date(1)'s default format"),
    ];
    for (conversion, what) in extensions {
        match refused(&[b'%', *conversion], &when) {
            StrftimeError::ExtensionSpecifier { specifier, .. } => assert_eq!(
                specifier, *conversion,
                "%{} is a GNU/BSD extension ({what}) outside C99 and POSIX",
                *conversion as char
            ),
            other => panic!("%{}: {other:?}", *conversion as char),
        }
    }
    // An unknown conversion is an error, NOT the two bytes copied through. A tzcode-derived
    // strftime copies them; doing that here would hand the guest the literal text `%Q` where it
    // asked for a time, and nothing downstream can tell that from a time.
    for unknown in [b'Q', b'q', b'1', b'i', b'f', b'N', b':', 0x00, 0xFF] {
        assert_eq!(
            refused(&[b'%', unknown], &when),
            StrftimeError::UnknownSpecifier { specifier: unknown },
            "%{unknown:#04x} must refuse rather than be copied through"
        );
    }
    // A format string that ends inside a specification.
    assert_eq!(refused(b"%", &when), StrftimeError::IncompleteSpecifier, "a trailing %");
    assert_eq!(refused(b"today: %", &when), StrftimeError::IncompleteSpecifier);
    assert_eq!(refused(b"%E", &when), StrftimeError::IncompleteSpecifier, "a trailing modifier");
    assert_eq!(refused(b"%O", &when), StrftimeError::IncompleteSpecifier);
    // And the refusal abandons the whole call: no partial timestamp comes back, because a
    // partial timestamp reads like a whole one.
    assert!(
        strftime(4096, b"%Y-%m-%d %k", &when).is_err(),
        "a refusal late in the format must refuse the call, not return the prefix"
    );
}

/// The return value: the byte count, or **zero with an indeterminate buffer**, at the exact
/// boundary.
///
/// C99 7.23.3.5 paragraph 4, quoted in [`strftime`]'s documentation: the count is returned "if
/// the total number of resulting characters including the terminating null character is not more
/// than maxsize", and otherwise "zero is returned and the contents of the array are
/// indeterminate". So four bytes of output need a `max` of **5**, not 4.
///
/// The believable wrong answer is `snprintf`'s contract — truncate to `max - 1` and return the
/// truncated length — which would put `2026-09-2` in the buffer. That is a date.
#[test]
fn the_result_needs_room_for_its_nul_and_otherwise_produces_nothing_at_all() {
    let when = utc(MONDAY_AFTERNOON);
    // "2026-09-21" is ten bytes, so eleven are needed.
    assert_eq!(
        strftime(11, b"%F", &when),
        Ok(StrftimeOutput::Fits(b"2026-09-21".to_vec())),
        "ten bytes and a NUL fit in eleven"
    );
    assert_eq!(
        strftime(10, b"%F", &when),
        Ok(StrftimeOutput::DoesNotFit { needed: 10 }),
        "ten bytes do NOT fit in ten: the NUL needs the eleventh"
    );
    assert_eq!(
        strftime(9, b"%F", &when),
        Ok(StrftimeOutput::DoesNotFit { needed: 10 }),
        "and nothing is produced — not a nine-byte prefix"
    );
    assert_eq!(strftime(0, b"%F", &when), Ok(StrftimeOutput::DoesNotFit { needed: 10 }));
    assert_eq!(
        strftime(usize::MAX, b"%F", &when),
        Ok(StrftimeOutput::Fits(b"2026-09-21".to_vec())),
        "a max of SIZE_MAX must not overflow the fit test"
    );
    // The empty result, which is the case C's interface famously cannot distinguish: strftime
    // returns 0 both when it wrote nothing and when nothing fitted. Here they are different
    // values, and that is the point of the enum.
    assert_eq!(
        strftime(1, b"", &when),
        Ok(StrftimeOutput::Fits(Vec::new())),
        "an empty result and its NUL fit in one byte: C returns 0 and the buffer holds \"\""
    );
    assert_eq!(
        strftime(0, b"", &when),
        Ok(StrftimeOutput::DoesNotFit { needed: 0 }),
        "the same C return value, 0, with an indeterminate buffer instead"
    );
    // The boundary walked one byte at a time over a longer format, so an off-by-one in the
    // comparison cannot hide in a single example.
    let full = rendered(b"%c", &when);
    for max in 0..=full.len() + 2 {
        let expected = if max > full.len() {
            StrftimeOutput::Fits(full.as_bytes().to_vec())
        } else {
            StrftimeOutput::DoesNotFit { needed: full.len() }
        };
        assert_eq!(strftime(max, b"%c", &when), Ok(expected), "max {max} of {} bytes", full.len());
    }
}

/// A `struct tm` field outside the range POSIX fixes for it is refused **by the conversions that
/// read it, and by those only**.
///
/// POSIX `<time.h>` gives the ranges; they are quoted in [`strftime`]'s documentation. The
/// second half of this test is the half that matters: a conversion that does not read the broken
/// field still formats, which is what stops "refuse everything" from passing.
#[test]
fn an_out_of_range_field_is_refused_by_the_conversions_that_read_it() {
    struct Row {
        field: &'static str,
        value: i32,
        lower: i32,
        upper: i32,
        readers: &'static [u8],
        /// A conversion that does not read this field and must still work.
        bystander: u8,
    }
    let rows = &[
        Row { field: "tm_sec", value: 61, lower: 0, upper: 60, readers: b"S", bystander: b'M' },
        Row { field: "tm_sec", value: -1, lower: 0, upper: 60, readers: b"S", bystander: b'H' },
        Row { field: "tm_min", value: 60, lower: 0, upper: 59, readers: b"M", bystander: b'S' },
        Row { field: "tm_hour", value: 24, lower: 0, upper: 23, readers: b"HIp", bystander: b'M' },
        Row { field: "tm_mday", value: 0, lower: 1, upper: 31, readers: b"de", bystander: b'm' },
        Row { field: "tm_mday", value: 32, lower: 1, upper: 31, readers: b"de", bystander: b'm' },
        Row { field: "tm_mon", value: 12, lower: 0, upper: 11, readers: b"bBhm", bystander: b'd' },
        Row { field: "tm_mon", value: 10_000, lower: 0, upper: 11, readers: b"bBhm", bystander: b'Y' },
        Row { field: "tm_wday", value: 7, lower: 0, upper: 6, readers: b"aAuwUWVGg", bystander: b'd' },
        Row { field: "tm_wday", value: -5, lower: 0, upper: 6, readers: b"aAuwUWVGg", bystander: b'H' },
        Row { field: "tm_yday", value: 366, lower: 0, upper: 365, readers: b"jUWVGg", bystander: b'd' },
        Row { field: "tm_yday", value: -1, lower: 0, upper: 365, readers: b"jUWVGg", bystander: b'Y' },
    ];
    for row in rows {
        let mut tm = MONDAY_AFTERNOON;
        match row.field {
            "tm_sec" => tm.sec = row.value,
            "tm_min" => tm.min = row.value,
            "tm_hour" => tm.hour = row.value,
            "tm_mday" => tm.mday = row.value,
            "tm_mon" => tm.mon = row.value,
            "tm_wday" => tm.wday = row.value,
            "tm_yday" => tm.yday = row.value,
            other => panic!("the row names a field this test does not set: {other}"),
        }
        let when = utc(tm);
        for &conversion in row.readers {
            assert_eq!(
                refused(&[b'%', conversion], &when),
                StrftimeError::FieldOutOfRange {
                    specifier: conversion,
                    field: row.field,
                    value: row.value,
                    lower: row.lower,
                    upper: row.upper,
                },
                "%{} reads {} and must refuse {}",
                conversion as char,
                row.field,
                row.value
            );
        }
        // NOT an empty arm and not decoration: without it this whole test is satisfied by an
        // implementation that refuses every call.
        assert!(
            !rendered(&[b'%', row.bystander], &when).is_empty(),
            "%{} does not read {} and must still format",
            row.bystander as char,
            row.field
        );
    }
    // 60 seconds is allowed, because POSIX bounds tm_sec at 60 so a positive leap second is
    // representable. Refusing it would be the over-careful mistake.
    let mut leap_second = MONDAY_AFTERNOON;
    leap_second.sec = 60;
    assert_eq!(rendered(b"%S", &utc(leap_second)), "60", "POSIX bounds tm_sec at [0, 60]");
    // tm_isdst is read by no conversion here, so any value formats.
    let mut odd_isdst = MONDAY_AFTERNOON;
    odd_isdst.isdst = 7;
    assert_eq!(rendered(b"%F %T %Z", &utc(odd_isdst)), "2026-09-21 14:05:09 UTC");
}

/// The year conversions are defined over the range the standards fix and refuse outside it.
///
/// `%C` and `%y` are two digits by definition, so they cover [0, 9999]. `%Y` and `%G` have no
/// standard width, so they cover [1000, 9999] — where ISO 8601 fixes four digits — and refuse
/// elsewhere rather than inventing a padding. [`strftime`]'s documentation has the argument; this
/// test pins both edges of both ranges so that widening or narrowing one is visible.
#[test]
fn the_year_conversions_refuse_a_year_whose_representation_no_standard_fixes() {
    let with_year = |tm_year: i32| {
        let mut tm = NEW_YEARS_DAY;
        tm.year = tm_year;
        utc(tm)
    };
    // The inclusive edges that must work.
    assert_eq!(rendered(b"%Y", &with_year(-900)), "1000", "tm_year -900 is the year 1000");
    assert_eq!(rendered(b"%Y", &with_year(8099)), "9999");
    assert_eq!(rendered(b"%C%y", &with_year(-1900)), "0000", "the year 0: century 00, year 00");
    assert_eq!(rendered(b"%C%y", &with_year(8099)), "9999");
    assert_eq!(rendered(b"%C%y", &with_year(-901)), "0999", "the year 999 has a century of 09");
    // The exclusive edges that must refuse, with the year carried so the message is actionable.
    assert_eq!(
        refused(b"%Y", &with_year(-901)),
        StrftimeError::YearNotRepresentable {
            specifier: b'Y',
            year: 999,
            lower: 1000,
            upper: 9999
        },
        "999 as %Y is `999` or `0999` and no standard says which"
    );
    assert_eq!(
        refused(b"%Y", &with_year(8100)),
        StrftimeError::YearNotRepresentable {
            specifier: b'Y',
            year: 10_000,
            lower: 1000,
            upper: 9999
        }
    );
    assert_eq!(
        refused(b"%C", &with_year(-1901)),
        StrftimeError::YearNotRepresentable {
            specifier: b'C',
            year: -1,
            lower: 0,
            upper: 9999
        },
        "the year -1 has no two-digit century"
    );
    assert_eq!(
        refused(b"%y", &with_year(-1901)),
        StrftimeError::YearNotRepresentable { specifier: b'y', year: -1, lower: 0, upper: 9999 }
    );
    // The two extremes of the field itself. tm_year is an int and the guest may set any of them;
    // the year in the error is the widened `tm_year + 1900`, which is the addition that would
    // have overflowed had it been done in i32 — in a release build, silently.
    assert_eq!(
        refused(b"%Y", &with_year(i32::MAX)),
        StrftimeError::YearNotRepresentable {
            specifier: b'Y',
            year: 2_147_485_547,
            lower: 1000,
            upper: 9999
        },
        "i32::MAX + 1900 must be computed in i64, not wrapped into a negative year"
    );
    assert_eq!(
        refused(b"%Y", &with_year(i32::MIN)),
        StrftimeError::YearNotRepresentable {
            specifier: b'Y',
            year: -2_147_481_748,
            lower: 1000,
            upper: 9999
        }
    );
    // %G and %g carry the *ISO* year, which for this tm (1 January, a Thursday) is the calendar
    // year — so the refusal names 10000 rather than the tm_year that produced it.
    assert_eq!(
        refused(b"%G", &with_year(8100)),
        StrftimeError::YearNotRepresentable {
            specifier: b'G',
            year: 10_000,
            lower: 1000,
            upper: 9999
        }
    );
    // And a conversion that does not read the year still works at both extremes, so the refusals
    // above are not an implementation that refuses everything.
    assert_eq!(rendered(b"%H:%M:%S", &with_year(i32::MIN)), "00:00:00");
    assert_eq!(rendered(b"%V", &with_year(i32::MAX)), "01", "%V needs no representable year");
}

/// `%z` and `%Z` read the two BSD extension fields at the end of bionic's `struct tm`.
#[test]
fn percent_z_reads_tm_gmtoff_and_percent_z_upper_reads_tm_zone() {
    let zoned = |gmtoff, zone| StrftimeTm { tm: MONDAY_AFTERNOON, gmtoff, zone };
    // POSIX: "the offset from UTC in the ISO 8601:2000 standard format (+hhmm or -hhmm)".
    let rows: &[(i64, &str, &str)] = &[
        (0, "+0000", "UTC is +0000; C has no negative zero here"),
        (3_600, "+0100", "one hour east"),
        (-18_000, "-0500", "five hours west, which is US Eastern standard time"),
        (19_800, "+0530", "five and a half hours: the minutes field is not always 00"),
        (-1_800, "-0030", "half an hour west"),
        (86_400, "+2400", "POSIX's TZ grammar bounds the hour field at 24, so this is in range"),
        (-86_400, "-2400", "and so is its negative"),
        (-1, "-0000", "a sub-minute offset truncates: the format has no room for seconds"),
    ];
    for (gmtoff, expected, why) in rows {
        assert_eq!(rendered(b"%z", &zoned(*gmtoff, None)), *expected, "{gmtoff}: {why}");
    }
    // Out of range, including the two values that would make a negation overflow.
    for gmtoff in [86_401i64, -86_401, i64::MAX, i64::MIN] {
        assert_eq!(
            refused(b"%z", &zoned(gmtoff, None)),
            StrftimeError::GmtoffOutOfRange { gmtoff },
            "{gmtoff} is not a UTC offset and must not be printed as one"
        );
    }
    // %Z is tm_zone's bytes, verbatim.
    assert_eq!(rendered(b"%Z", &zoned(0, Some(b"UTC"))), "UTC");
    assert_eq!(rendered(b"%Z", &zoned(-18_000, Some(b"EST"))), "EST");
    assert_eq!(
        rendered(b"[%Z]", &zoned(0, Some(b""))),
        "[]",
        "an empty zone name emits nothing, which is a result rather than a failure"
    );
    // A NULL tm_zone sends a real strftime to the global tzname, which this crate does not have.
    // "UTC" would be the plausible stub: right for every tm this project makes today.
    assert_eq!(
        refused(b"%Z", &zoned(0, None)),
        StrftimeError::TimeZoneNameUnavailable,
        "a NULL tm_zone must refuse rather than assume UTC"
    );
    // ...and %z beside it still works, because it reads the other field.
    assert_eq!(rendered(b"%z", &zoned(0, None)), "+0000");
}

/// Literal bytes are copied through unchanged, including bytes that are not valid UTF-8.
///
/// This is why the interface is `&[u8]` and not `&str`: the only total `&[u8]` to `&str`
/// conversion is lossy, and it would replace the byte below with U+FFFD — three bytes of
/// replacement character where C copies one byte. A wrong-text failure introduced by a
/// signature.
#[test]
fn literal_bytes_including_invalid_utf8_are_copied_through_unchanged() {
    let when = utc(MONDAY_AFTERNOON);
    let format = b"\xC3\x28 %Y \xFF\xFE";
    let StrftimeOutput::Fits(bytes) = strftime(64, format, &when).expect("no conversion refuses")
    else {
        panic!("64 bytes is room enough");
    };
    assert_eq!(bytes, b"\xC3\x28 2026 \xFF\xFE".to_vec());
    // Two invalid bytes, a space, four digits, a space and two more: ten bytes. Counted in
    // bytes, which is what C counts — a `char`-based count of the same output gives eight.
    assert_eq!(bytes.len(), 10, "ten bytes, which is what C would return");
    // A byte that happens to be a conversion character is still a literal when no % precedes it.
    assert_eq!(rendered(b"Y m d", &when), "Y m d");
}

/// A format string that would produce more than the cap refuses, rather than allocating what the
/// caller did not size.
///
/// `%c` is 24 bytes from two, the widest expansion here, so 50,000 of them is about 1.2 MB —
/// past [`MAX_STRFTIME_OUTPUT`]. The cap is checked once per format byte, so the refusal arrives
/// a little past the limit rather than exactly at it; that is documented on the constant and is
/// why this asserts the error rather than the length.
#[test]
fn a_format_that_would_exceed_the_output_cap_refuses() {
    let when = utc(MONDAY_AFTERNOON);
    let mut format = Vec::new();
    for _ in 0..50_000 {
        format.extend_from_slice(b"%c");
    }
    assert_eq!(
        strftime(usize::MAX, &format, &when),
        Err(StrftimeError::OutputTooLarge { limit: MAX_STRFTIME_OUTPUT }),
        "a caller-sized allocation is the caller's to refuse"
    );
    // And the cap is high enough that nothing the thunk boundary can deliver reaches it: its
    // `STRING_LIMIT` is 64 KiB, so a 64 KiB format of literals — the longest format string that
    // can arrive at all — must format. This is the half of the property that a cap tightened
    // "to be safe" would break, and the half a test of the refusal alone would not see.
    let longest_the_boundary_can_deliver = vec![b'x'; 64 * 1024];
    assert!(
        matches!(
            strftime(usize::MAX, &longest_the_boundary_can_deliver, &when),
            Ok(StrftimeOutput::Fits(_))
        ),
        "the cap must sit above every format string the boundary can hand over"
    );
}

/// **No `struct tm` and no format byte at all can make this panic**, which is Global Constraint
/// 11 for a function whose every input the guest chose.
///
/// n = 9 hostile `tm`s x 4 `tm_gmtoff` values x 256 format bytes x 3 modifier forms = 27,648
/// calls, every one of which must return. This is a sample of the field *extremes*, not an
/// enumeration of the field space, and it is stated as one: `docs/VERIFICATION.md` entry 3 is
/// about a test whose `Err` arm asserted nothing, so the arms here assert what each outcome has
/// to look like.
#[test]
fn no_struct_tm_and_no_format_byte_can_make_this_panic() {
    let hostile = [
        MONDAY_AFTERNOON,
        Tm { sec: 0, min: 0, hour: 0, mday: 1, mon: 0, year: i32::MIN, wday: 0, yday: 0, isdst: 0 },
        Tm {
            sec: i32::MAX,
            min: i32::MAX,
            hour: i32::MAX,
            mday: i32::MAX,
            mon: i32::MAX,
            year: i32::MAX,
            wday: i32::MAX,
            yday: i32::MAX,
            isdst: i32::MAX,
        },
        Tm {
            sec: i32::MIN,
            min: i32::MIN,
            hour: i32::MIN,
            mday: i32::MIN,
            mon: i32::MIN,
            year: i32::MIN,
            wday: i32::MIN,
            yday: i32::MIN,
            isdst: i32::MIN,
        },
        Tm { sec: 60, min: 59, hour: 23, mday: 31, mon: 11, year: 8099, wday: 6, yday: 365, isdst: -1 },
        Tm { sec: 0, min: 0, hour: 0, mday: 1, mon: 10_000, year: 0, wday: -5, yday: -1, isdst: 0 },
        Tm { sec: -1, min: -1, hour: -1, mday: 0, mon: -1, year: -1900, wday: -1, yday: -1, isdst: 0 },
        // Fields inside their ranges but mutually inconsistent: day 366 of a year whose 1 January
        // the weekday says is a Thursday. The answer must be defined, not necessarily sensible.
        Tm { sec: 0, min: 0, hour: 0, mday: 31, mon: 11, year: 125, wday: 0, yday: 365, isdst: 0 },
        EPOCH,
    ];
    let offsets = [0i64, i64::MIN, i64::MAX, 86_400];
    for tm in hostile {
        for gmtoff in offsets {
            for zone in [None, Some(&b"UTC"[..])] {
                let when = StrftimeTm { tm, gmtoff, zone };
                for byte in 0u8..=255 {
                    for form in [
                        vec![b'%', byte],
                        vec![b'%', b'E', byte],
                        vec![b'%', b'O', byte],
                    ] {
                        match strftime(4096, &form, &when) {
                            // Not an empty arm: a conversion that succeeds must have produced
                            // something that fits in a field, which is the property a wrapped
                            // or unchecked computation would break.
                            Ok(StrftimeOutput::Fits(bytes)) => assert!(
                                bytes.len() <= 32,
                                "%{} produced {} bytes from a two-byte conversion",
                                byte as char,
                                bytes.len()
                            ),
                            Ok(StrftimeOutput::DoesNotFit { needed }) => panic!(
                                "%{} needed {needed} bytes, which cannot fit in 4096",
                                byte as char
                            ),
                            // Also not empty: every refusal must name something, and the one
                            // thing it must never be is a cap that a two-byte format reached.
                            Err(error) => assert!(
                                !matches!(error, StrftimeError::OutputTooLarge { .. }),
                                "%{} hit the output cap from two bytes: {error}",
                                byte as char
                            ),
                        }
                    }
                }
            }
        }
    }
}

/// `read_tm` decodes the same 56 bytes `write_tm` produces, field for field.
///
/// The round trip is a **consistency** check between two functions in this module, not evidence
/// that the layout is right — the layout's evidence is `TM_BYTES`'s derivation from bionic's
/// `<time.h>`, and it is recorded there as unverified against an NDK. What this does catch is the
/// two of them drifting apart, and the offsets are asserted against the documented numbers rather
/// than against each other.
#[test]
fn read_tm_decodes_the_layout_write_tm_produces() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[0u8; 56]);
    let tm = gmtime(1_789_000_000).expect("2026-09-10T00:26:40Z");
    write_tm(&mut mem, 0x1000, &tm, 0xDEAD_BEEF).expect("a mapped struct tm");
    let read = read_tm(&mem, 0x1000).expect("the same 56 bytes");
    assert_eq!(read.tm, tm, "the nine POSIX fields survive the round trip");
    assert_eq!(read.gmtoff, 0, "write_tm writes UTC's offset, which is zero");
    assert_eq!(read.zone, 0xDEAD_BEEF, "tm_zone is a pointer, returned unresolved");
    // The hostile destinations, matching write_tm's own test.
    assert!(read_tm(&mem, 0).is_err(), "a null struct tm");
    assert!(read_tm(&mem, u64::MAX - 8).is_err(), "a wrapping struct tm");
    let mut short = MockMemory::new();
    short.map(0x2000, &[0u8; 55]);
    assert!(read_tm(&short, 0x2000).is_err(), "one byte short of the structure must fault");
    // Fields are returned exactly as found, including values POSIX does not allow — the ranges
    // belong to the conversions, not to the decoder.
    let mut nonsense = MockMemory::new();
    nonsense.map(0x3000, &[0xFFu8; 56]);
    let read = read_tm(&nonsense, 0x3000).expect("mapped");
    assert_eq!(read.tm.sec, -1, "0xFFFFFFFF little-endian as an int is -1");
    assert_eq!(read.gmtoff, -1);
    assert_eq!(read.zone, u64::MAX);
}

/// `gmtime` into `strftime`: the one path the adapter will actually take, end to end.
///
/// The timestamp and its date come from `time.rs`'s own table, which derives them from the
/// calendar rather than from another implementation.
#[test]
fn a_gmtime_result_formats_as_the_iso_8601_timestamp_it_is() {
    let tm = gmtime(1_789_000_000).expect("in range");
    let when = StrftimeTm { tm, gmtoff: 0, zone: Some(b"UTC") };
    assert_eq!(rendered(b"%Y-%m-%dT%H:%M:%SZ", &when), "2026-09-10T00:26:40Z");
    assert_eq!(rendered(b"%FT%T%z", &when), "2026-09-10T00:26:40+0000");
    assert_eq!(rendered(b"%a %d %b %Y %T %Z", &when), "Thu 10 Sep 2026 00:26:40 UTC");
    // The epoch, from gmtime rather than from the constant above — so the constant is checked
    // against the calendar code as well as the other way round.
    let epoch = gmtime(0).expect("the epoch");
    assert_eq!(epoch, EPOCH, "the reference tm and gmtime(0) must be the same instant");
    assert_eq!(rendered(b"%c", &StrftimeTm { tm: epoch, gmtoff: 0, zone: Some(b"UTC") }),
        "Thu Jan  1 00:00:00 1970");
}
