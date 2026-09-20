//! A log sink: somewhere a line the guest wrote can go.
//!
//! Serves the guest's `__android_log_print`, `syslog`, `openlog` and `closelog`. On a real device
//! those reach `logd` over a socket and `/dev/log`; there is no `logd` here, so the sink is the
//! host's standard error, one line per record.
//!
//! # Why this is a seam at all, when it is four lines of `eprintln!`
//!
//! Because the destination is the part that changes. A desktop host wants stderr; a host embedding
//! Omnidroid in a UI wants a callback; a test wants to assert on what was logged without a suite
//! that prints thousands of lines. Putting the *formatting* here — one place that decides what a
//! record looks like — means those three destinations cannot disagree about the format, and means
//! the guest's priority and tag survive to whoever is reading rather than being flattened into a
//! message string at the first hop.
//!
//! **Policy is not here.** Which records are emitted, whether they are also captured, and what to
//! do about a guest that logs in a loop are all the caller's; `omni-android`'s adapter owns them,
//! per instance, because the runtime hosts several guest instances and a process-wide sink would
//! merge their output into one stream with nothing to tell them apart.
//!
//! # Five targets
//!
//! No backend and no `cfg`: `std::io::Write` on `std::io::stderr()` is portable standard library.
//! The argument for not manufacturing an `Unsupported` arm is the one [`crate::clock`] sets out —
//! a refusal here would be a false claim in the other direction. What is *not* claimed is any
//! behaviour beyond "the bytes reach the host's stderr": there is no integration with `logd`,
//! `syslogd`, the macOS unified log or the Windows event log on any target.

use std::io::Write;

/// Android's log priority, as `__android_log_print`'s first argument spells it.
///
/// The numbers are `android_LogPriority` from `<android/log.h>`, which is a published part of the
/// NDK ABI. They are matched **exactly** rather than ranged, because a guest that passes a number
/// outside this set is saying something this layer does not understand, and mapping it to the
/// nearest neighbour would be an invention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(i32)]
pub enum Priority {
    /// `ANDROID_LOG_UNKNOWN`.
    Unknown = 0,
    /// `ANDROID_LOG_DEFAULT`. Never written by a caller; means "use the default".
    Default = 1,
    /// `ANDROID_LOG_VERBOSE`.
    Verbose = 2,
    /// `ANDROID_LOG_DEBUG`.
    Debug = 3,
    /// `ANDROID_LOG_INFO`.
    Info = 4,
    /// `ANDROID_LOG_WARN`.
    Warn = 5,
    /// `ANDROID_LOG_ERROR`.
    Error = 6,
    /// `ANDROID_LOG_FATAL`. **Logging at this level does not abort**: on Android it is
    /// `__android_log_assert` that aborts, not `__android_log_print`.
    Fatal = 7,
    /// `ANDROID_LOG_SILENT`.
    Silent = 8,
}

impl Priority {
    /// The `android_LogPriority` value, or `None` for a number that is not one.
    ///
    /// `None` is a refusal the caller turns into a named error, not a fallback to
    /// [`Priority::Unknown`]: a guest passing 42 has a bug or is hostile, and answering it with a
    /// priority it did not ask for hides both.
    #[must_use]
    pub const fn from_android(value: i32) -> Option<Priority> {
        Some(match value {
            0 => Priority::Unknown,
            1 => Priority::Default,
            2 => Priority::Verbose,
            3 => Priority::Debug,
            4 => Priority::Info,
            5 => Priority::Warn,
            6 => Priority::Error,
            7 => Priority::Fatal,
            8 => Priority::Silent,
            _ => return None,
        })
    }

    /// The severity half of a `syslog` priority, mapped onto this scale.
    ///
    /// `syslog(3)`'s argument is `facility | severity`, where the severity is the low three bits
    /// (`LOG_EMERG` 0 … `LOG_DEBUG` 7) and the facility is the rest. The two scales genuinely
    /// correspond — they are both "how bad is this" — so this is a mapping rather than a guess,
    /// and the facility is dropped by the caller after being *reported*, never silently.
    #[must_use]
    pub const fn from_syslog_severity(severity: i32) -> Option<Priority> {
        Some(match severity {
            // LOG_EMERG, LOG_ALERT, LOG_CRIT: nothing above Fatal exists on the Android scale.
            0..=2 => Priority::Fatal,
            3 => Priority::Error,   // LOG_ERR
            4 => Priority::Warn,    // LOG_WARNING
            5 => Priority::Info,    // LOG_NOTICE
            6 => Priority::Info,    // LOG_INFO
            7 => Priority::Debug,   // LOG_DEBUG
            _ => return None,
        })
    }

    /// The one-letter marker Android's own `logcat` uses for this priority.
    #[must_use]
    pub const fn marker(self) -> char {
        match self {
            Priority::Unknown => '?',
            Priority::Default => 'D',
            Priority::Verbose => 'V',
            Priority::Debug => 'D',
            Priority::Info => 'I',
            Priority::Warn => 'W',
            Priority::Error => 'E',
            Priority::Fatal => 'F',
            Priority::Silent => 'S',
        }
    }
}

/// One line the guest wrote.
///
/// Borrows rather than owning: the caller has the bytes already, and a record that allocated would
/// allocate on a path a guest can drive in a loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// How bad the guest says it is.
    pub priority: Priority,
    /// The guest's tag, or `""` when it passed none.
    pub tag: &'a str,
    /// The formatted message, without a trailing newline.
    pub message: &'a str,
}

/// Render a record the way `logcat` renders one: `P/tag: message`.
///
/// Newlines inside `message` are **kept**, not escaped: a guest that logs a multi-line message
/// meant to log one, and mangling it would make the log less useful than the thing it describes.
/// The caller appends exactly one line terminator.
#[must_use]
pub fn format_line(record: &Record<'_>) -> String {
    let marker = record.priority.marker();
    if record.tag.is_empty() {
        format!("{marker}/: {}", record.message)
    } else {
        format!("{marker}/{}: {}", record.tag, record.message)
    }
}

/// Write one record to the host's standard error.
///
/// **Infallible by design, and this is the one place in the crate where swallowing an error is
/// right.** The caller is a guest `__android_log_print`, whose C signature returns "bytes written"
/// and whose callers universally ignore it; a closed or redirected stderr is a fact about the host
/// that the guest can do nothing about and must not be failed for. A failure here is dropped,
/// which is what `logd` being unavailable does on a real device too.
pub fn emit(record: &Record<'_>) {
    let line = format_line(record);
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Android priority numbers are the NDK's, and nothing outside the set maps.
    #[test]
    fn the_android_priority_numbers_are_exactly_the_ndk_set() {
        assert_eq!(Priority::from_android(2), Some(Priority::Verbose));
        assert_eq!(Priority::from_android(7), Some(Priority::Fatal));
        assert_eq!(Priority::from_android(8), Some(Priority::Silent));
        for outside in [-1, 9, 42, i32::MIN, i32::MAX] {
            assert_eq!(
                Priority::from_android(outside),
                None,
                "{outside} is not an android_LogPriority and must not be mapped to a neighbour"
            );
        }
    }

    /// The syslog severities map onto the Android scale, and nothing above 7 does.
    #[test]
    fn the_syslog_severities_map_and_nothing_else_does() {
        assert_eq!(Priority::from_syslog_severity(0), Some(Priority::Fatal));
        assert_eq!(Priority::from_syslog_severity(3), Some(Priority::Error));
        assert_eq!(Priority::from_syslog_severity(7), Some(Priority::Debug));
        for outside in [-1, 8, i32::MAX] {
            assert_eq!(Priority::from_syslog_severity(outside), None);
        }
    }

    /// The rendered line carries the priority, the tag and the message, in that order.
    #[test]
    fn a_record_renders_as_logcat_renders_one() {
        let record = Record { priority: Priority::Warn, tag: "Roblox", message: "hello" };
        assert_eq!(format_line(&record), "W/Roblox: hello");
        let untagged = Record { priority: Priority::Info, tag: "", message: "hello" };
        assert_eq!(format_line(&untagged), "I/: hello");
        // A multi-line message survives intact.
        let multi = Record { priority: Priority::Error, tag: "t", message: "a\nb" };
        assert_eq!(format_line(&multi), "E/t: a\nb");
    }
}
