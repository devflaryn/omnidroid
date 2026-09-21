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
//!
//! # liblog's caps live here, and they are the platform's numbers rather than ours
//!
//! A real `__android_log_print` does not fail on an oversized message: it **truncates**, twice,
//! at two constants that are part of the log ABI. [`liblog_caps`] is that arithmetic and
//! [`Truncation`] is what it removed. They are in this module rather than in the adapter because
//! they are facts about Android's logger, not policy of the bionic layer — the same reason
//! [`Priority`] is here — and because the record that carries the result is
//! [`Record`]. Every constant names the AOSP file it was read out of; none of them is a guess.
//!
//! Reproducing the platform's truncation is **not** a stub, but a truncation nobody can see would
//! be: a log line silently missing its tail is a wrong answer its reader cannot detect. So a
//! truncated [`Record`] carries [`Truncation`] and [`format_line`] says so in the rendered line.

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

/// `LOG_BUF_SIZE` from `system/logging/liblog/logger_write.cpp` (AOSP `main`).
///
/// **The cap that actually governs `__android_log_print`, and it is not the one people quote.**
/// Read out of the source rather than remembered:
///
/// ```c
/// #define LOG_BUF_SIZE 1024
///
/// int __android_log_print(int prio, const char* tag, const char* fmt, ...) {
///   ...
///   __attribute__((uninitialized)) char buf[LOG_BUF_SIZE];
///   va_start(ap, fmt);
///   vsnprintf(buf, LOG_BUF_SIZE, fmt, ap);
/// ```
///
/// `vsnprintf` into a fixed 1024-byte buffer **truncates**; it does not fail and it does not
/// abort. That is the defined platform behaviour for an over-long message, so a layer that
/// refused one would be inventing a failure the device does not have. `__android_log_vprint`,
/// which is what bionic's own `syslog`/`vsyslog` call, has the identical body — so this cap
/// covers `syslog` too, and neither call has a second path around it.
pub const LOG_BUF_SIZE: usize = 1024;

/// `LOGGER_ENTRY_MAX_PAYLOAD` from `system/logging/liblog/include/log/log.h` (AOSP `main`), with
/// its own comment:
///
/// ```c
/// /*
///  * The maximum size of the log entry payload that can be
///  * written to the logger. An attempt to write more than
///  * this amount will result in a truncated log entry.
///  */
/// #define LOGGER_ENTRY_MAX_PAYLOAD 4068
/// ```
///
/// **Not derived from [`LOGGER_ENTRY_MAX_LEN`], and the arithmetic that looks like it should
/// derive it does not**: `5120 - sizeof(struct logger_entry)` is `5120 - 28 = 5092`, not 4068.
/// 4068 is `4096 - 28` — one page less the header as the header was when the number was chosen —
/// and it is a separate hard-coded constant in a different header. Deriving it would have given
/// a believable wrong answer 1024 bytes too large.
pub const LOGGER_ENTRY_MAX_PAYLOAD: usize = 4068;

/// `LOGGER_ENTRY_MAX_LEN` from `system/logging/liblog/include/log/log_read.h` (AOSP `main`).
///
/// **Recorded so it is not mistaken for the write cap**, which is the mistake the note on
/// [`LOGGER_ENTRY_MAX_PAYLOAD`] exists to stop. Its own comment says what it is: "the maximum
/// size of a log entry which can be **read**". The effective payload a writer may send is the
/// smaller, independent number. Nothing here uses this one; it is documentation with a type.
pub const LOGGER_ENTRY_MAX_LEN: usize = 5 * 1024;

/// Bytes of one payload that are neither tag nor message.
///
/// `liblog`'s writer builds exactly three `iovec`s (`logger_write.cpp`,
/// `__android_log_write_log_message`):
///
/// ```c
/// vec[0].iov_len = 1;                                    // the priority byte
/// vec[1].iov_len = strlen(log_message->tag) + 1;         // tag and its NUL
/// vec[2].iov_len = strlen(log_message->message) + 1;     // message and its NUL
/// ```
///
/// so the framing is one priority byte and two terminators: three.
pub const PAYLOAD_FRAMING_BYTES: usize = 3;

/// The most tag-plus-message bytes one record may carry: **4065**.
///
/// [`LOGGER_ENTRY_MAX_PAYLOAD`] less [`PAYLOAD_FRAMING_BYTES`]. This is the number a caller
/// budgeting host memory for one record should use, not 4068 and not 5120.
pub const MAX_TAG_AND_MESSAGE_BYTES: usize = LOGGER_ENTRY_MAX_PAYLOAD - PAYLOAD_FRAMING_BYTES;

/// The write cap is the smaller of the two `liblog` numbers, and a layer that reached for the
/// read cap would admit 1,052 bytes per record that a device would have cut. A compile-time
/// assertion rather than a test, because it is a relation between two constants that no input can
/// make false at run time (`VERIFICATION.md` entry 12).
const _: () = assert!(LOGGER_ENTRY_MAX_PAYLOAD < LOGGER_ENTRY_MAX_LEN);

/// The most message bytes one record may carry whatever the tag is: **1023**.
///
/// [`LOG_BUF_SIZE`] less the NUL `vsnprintf` writes. The payload cap is four times larger, so for
/// `__android_log_print` and `syslog` this is the cap that binds first and the payload cap only
/// ever takes bytes off the *tag*.
pub const MAX_MESSAGE_BYTES: usize = LOG_BUF_SIZE - 1;

/// How many bytes of a tag and a message `liblog` would keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kept {
    /// Bytes of the tag that survive.
    pub tag: usize,
    /// Bytes of the message that survive.
    pub message: usize,
}

/// What `liblog`'s caps removed from one record. Present only when something was removed.
///
/// The *surviving* lengths are the record's own `tag.len()` and `message.len()`; this carries
/// what they were before, because a reader needs both numbers to know how much is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Truncation {
    /// Bytes the tag had before the caps.
    pub tag_bytes: usize,
    /// Bytes the formatted message had before the caps.
    pub message_bytes: usize,
}

/// Apply `liblog`'s two caps, in `liblog`'s order, and report how much of each survives.
///
/// Both arguments are byte counts of **guest** bytes — a tag and a message are byte strings the
/// guest chose and are under no obligation to be UTF-8, so counting them as host `char`s would
/// cap a different thing than the device does.
///
/// The order matters and is the device's:
///
/// 1. `__android_log_print` formats with `vsnprintf(buf, LOG_BUF_SIZE, ...)`, so the **message**
///    is cut to [`MAX_MESSAGE_BYTES`] first and the tag is untouched at this step.
/// 2. `LogdWrite` then walks the three `iovec`s accumulating `payloadSize` and cuts the one that
///    crosses [`LOGGER_ENTRY_MAX_PAYLOAD`]:
///
/// ```c
/// if (payloadSize > LOGGER_ENTRY_MAX_PAYLOAD) {
///   newVec[i].iov_len -= payloadSize - LOGGER_ENTRY_MAX_PAYLOAD;
/// ```
///
/// The tag is `iov[1]` and the message is `iov[2]`, so the tag is filled first and the message
/// gets whatever is left — which is why the tag is capped before the message below, and not the
/// other way round. Capping them in the other order would keep a long message and throw away the
/// tag, which is a believable wrong answer: it looks like "the useful part survived".
///
/// **One deliberate difference from the device, stated rather than hidden.** `liblog`'s cut
/// shortens an `iovec` without re-adding the NUL, so a payload cut inside the tag reaches `logd`
/// unterminated. Here a tag and a message are `String`s in a host ring rather than bytes on a
/// wire, so the terminators are not modelled at all — only the space they occupy is, through
/// [`PAYLOAD_FRAMING_BYTES`]. Nothing a guest can observe depends on the difference, because
/// nothing reads this back into guest memory.
///
/// # Arithmetic
///
/// Both inputs are guest-chosen lengths, so every step is a `min` or a subtraction whose operand
/// was just clamped below the constant it is subtracted from — there is no `+` on a
/// guest-controlled value here to wrap in release (`VERIFICATION.md` entry 3).
#[must_use]
pub fn liblog_caps(tag_bytes: usize, message_bytes: usize) -> Kept {
    // Step 1: vsnprintf into `char buf[LOG_BUF_SIZE]`.
    let message = message_bytes.min(MAX_MESSAGE_BYTES);
    // Step 2: the payload, tag first because it is the earlier iovec.
    let tag = tag_bytes.min(MAX_TAG_AND_MESSAGE_BYTES);
    // `tag` is at most MAX_TAG_AND_MESSAGE_BYTES by the line above, so this cannot underflow.
    let message = message.min(MAX_TAG_AND_MESSAGE_BYTES - tag);
    Kept { tag, message }
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
    /// What [`liblog_caps`] removed, or `None` when the record arrived whole.
    ///
    /// Carried through to [`format_line`] rather than folded into `message`, so that an
    /// untruncated line is byte-identical to what `logcat` prints and a truncated one cannot be
    /// mistaken for a complete one.
    pub truncated: Option<Truncation>,
}

/// The text [`format_line`] appends to a record that lost bytes to [`liblog_caps`].
///
/// A constant so that a reader — a person or a test — can match on one string rather than on a
/// shape, and so that the mutation that removes the marker changes a name rather than a format
/// literal buried in a function.
pub const TRUNCATION_MARKER: &str = "omnidroid: truncated at liblog's caps";

/// Render a record the way `logcat` renders one: `P/tag: message`.
///
/// Newlines inside `message` are **kept**, not escaped: a guest that logs a multi-line message
/// meant to log one, and mangling it would make the log less useful than the thing it describes.
/// The caller appends exactly one line terminator.
///
/// # A truncated record says so, on the line
///
/// An untruncated record renders exactly as before — byte-identical to `logcat` — and a truncated
/// one gains a trailing `[omnidroid: truncated at liblog's caps — …]` naming both the surviving
/// and the original byte counts. Without it the only thing distinguishing "the guest logged this"
/// from "the guest logged this and 40 KiB more" is the absence of a tail, which is precisely the
/// wrong answer a reader cannot detect. The marker goes **after** the message rather than before
/// it so that a reader diffing two runs sees the messages line up.
#[must_use]
pub fn format_line(record: &Record<'_>) -> String {
    let marker = record.priority.marker();
    let tag = record.tag;
    let mut line = if tag.is_empty() {
        format!("{marker}/: {}", record.message)
    } else {
        format!("{marker}/{tag}: {}", record.message)
    };
    if let Some(cut) = record.truncated {
        // `write!` into a `String` cannot fail, so this is a `push_str` of a formatted piece
        // rather than a `write!` whose `Result` would have to be discarded.
        line.push_str(&format!(
            " [{TRUNCATION_MARKER} -- message {} of {} bytes, tag {} of {} bytes]",
            record.message.len(),
            cut.message_bytes,
            tag.len(),
            cut.tag_bytes
        ));
    }
    line
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
        let record =
            Record { priority: Priority::Warn, tag: "Roblox", message: "hello", truncated: None };
        assert_eq!(format_line(&record), "W/Roblox: hello");
        let untagged =
            Record { priority: Priority::Info, tag: "", message: "hello", truncated: None };
        assert_eq!(format_line(&untagged), "I/: hello");
        // A multi-line message survives intact.
        let multi = Record { priority: Priority::Error, tag: "t", message: "a\nb", truncated: None };
        assert_eq!(format_line(&multi), "E/t: a\nb");
    }

    /// **The three `liblog` constants are the ones AOSP defines, by value.**
    ///
    /// Pinned as numbers and not only as names, because a test written against the names alone
    /// passes for any value at all — including the plausible wrong one this module's own doc
    /// comment warns about, `LOGGER_ENTRY_MAX_LEN - sizeof(struct logger_entry)` = 5092.
    ///
    /// Source, read 2026-09-21 from AOSP `main`:
    /// `system/logging/liblog/logger_write.cpp` (`LOG_BUF_SIZE`),
    /// `system/logging/liblog/include/log/log.h` (`LOGGER_ENTRY_MAX_PAYLOAD`),
    /// `system/logging/liblog/include/log/log_read.h` (`LOGGER_ENTRY_MAX_LEN`).
    #[test]
    fn the_liblog_constants_are_aosps_numbers_and_the_derived_one_is_not_the_entry_length() {
        assert_eq!(LOG_BUF_SIZE, 1024);
        assert_eq!(LOGGER_ENTRY_MAX_PAYLOAD, 4068);
        assert_eq!(LOGGER_ENTRY_MAX_LEN, 5120);
        assert_eq!(PAYLOAD_FRAMING_BYTES, 3);
        assert_eq!(MAX_MESSAGE_BYTES, 1023);
        assert_eq!(MAX_TAG_AND_MESSAGE_BYTES, 4065);
        // The payload cap is NOT the entry length less the header, and 4068 is the smaller of
        // the two. A layer that derived it would admit 1024 bytes per record it may not send.
        let sizeof_logger_entry = 28;
        assert_ne!(LOGGER_ENTRY_MAX_PAYLOAD, LOGGER_ENTRY_MAX_LEN - sizeof_logger_entry);
        // That the payload cap is the smaller of the two is asserted at compile time, beside the
        // constants themselves.
        // 4068 is one page less the same header, which is where the number came from.
        assert_eq!(LOGGER_ENTRY_MAX_PAYLOAD, 4096 - sizeof_logger_entry);
    }

    /// **`liblog_caps` cuts the message first and the tag second**, and every named case is
    /// asserted by name rather than by counting how many were cut.
    ///
    /// Membership, not totals (`VERIFICATION.md` entry 1): each row below names the input and the
    /// exact pair of surviving lengths, so a cap that clamped the wrong one of the two — which is
    /// the believable wrong answer, because "keep the message, drop the tag" reads as keeping the
    /// useful part — fails on the row that distinguishes them rather than on a count.
    #[test]
    fn liblog_caps_cuts_the_message_at_the_buffer_and_the_tag_at_the_payload() {
        // Nothing to cut.
        assert_eq!(liblog_caps(6, 8), Kept { tag: 6, message: 8 });
        // Exactly at the message cap, and one past it.
        assert_eq!(liblog_caps(6, MAX_MESSAGE_BYTES), Kept { tag: 6, message: MAX_MESSAGE_BYTES });
        assert_eq!(
            liblog_caps(6, MAX_MESSAGE_BYTES + 1),
            Kept { tag: 6, message: MAX_MESSAGE_BYTES }
        );
        // A megabyte of message: capped by the 1024-byte stack buffer, not by the payload.
        assert_eq!(liblog_caps(0, 1 << 20), Kept { tag: 0, message: MAX_MESSAGE_BYTES });
        // A tag longer than the whole payload takes every byte, and the message gets none --
        // the tag is `iov[1]` and is filled first.
        assert_eq!(
            liblog_caps(64 * 1024, 100),
            Kept { tag: MAX_TAG_AND_MESSAGE_BYTES, message: 0 }
        );
        // A tag that leaves less room than the message cap: the message takes the remainder.
        let tag = MAX_TAG_AND_MESSAGE_BYTES - 10;
        assert_eq!(liblog_caps(tag, 500), Kept { tag, message: 10 });
        // The invariant the ring's byte bound is built on: whatever the inputs, the sum fits.
        for (t, m) in [(0, 0), (1, 1), (5000, 5000), (usize::MAX, usize::MAX), (4065, 0)] {
            let kept = liblog_caps(t, m);
            assert!(
                kept.tag + kept.message <= MAX_TAG_AND_MESSAGE_BYTES,
                "({t}, {m}) kept {kept:?}, which does not fit the payload"
            );
            assert!(kept.tag <= t && kept.message <= m, "({t}, {m}) grew to {kept:?}");
            assert!(kept.message <= MAX_MESSAGE_BYTES, "({t}, {m}) kept {kept:?}");
        }
    }

    /// **A truncated record says so in the rendered line, and an untruncated one is unchanged.**
    ///
    /// The second half is the load-bearing one: the marker must not appear on a whole record, or
    /// it says nothing when it does appear.
    #[test]
    fn a_truncated_record_says_so_and_an_untruncated_one_is_byte_identical() {
        let whole =
            Record { priority: Priority::Info, tag: "Roblox", message: "hello", truncated: None };
        assert_eq!(format_line(&whole), "I/Roblox: hello");
        assert!(!format_line(&whole).contains(TRUNCATION_MARKER));

        let cut = Record {
            priority: Priority::Info,
            tag: "Roblox",
            message: "hel",
            truncated: Some(Truncation { tag_bytes: 6, message_bytes: 1_048_576 }),
        };
        let line = format_line(&cut);
        assert!(line.starts_with("I/Roblox: hel"), "{line}");
        assert!(line.contains(TRUNCATION_MARKER), "{line}");
        assert!(line.contains("message 3 of 1048576 bytes"), "{line}");
        assert!(line.contains("tag 6 of 6 bytes"), "{line}");
    }
}
