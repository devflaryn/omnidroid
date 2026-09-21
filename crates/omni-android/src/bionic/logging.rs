//! `liblog` and `syslog`: `__android_log_print`, `syslog`, `openlog`, `closelog`.
//!
//! # These are the four that must **not** be refused
//!
//! Everything else in this phase that cannot be modelled refuses by name. Logging is the opposite
//! case, and the asymmetry is deliberate: a log call has **no return value the guest acts on**
//! (`syslog` returns `void`; `__android_log_print`'s byte count is universally ignored), so there
//! is no believable wrong answer to give. What there is instead is an enormous amount of
//! information — the engine's own account of what it is doing, arriving in order, during the run of
//! 3,594 initializers that this milestone has to get through. Refusing `__android_log_print` would
//! halt the run at the first thing the engine wanted to tell us.
//!
//! So these are serviced, and the formatting is the real `printf` engine rather than a passthrough
//! of the format string: a line reading `%s at %p` with the arguments dropped is worse than no line.
//!
//! # Two sinks, and why the capture is not just for tests
//!
//! Every record goes to the instance's bounded ring ([`Bionic::log_records`](super::Bionic::log_records))
//! and, when the instance has it enabled, to the host's standard error through
//! [`omni_platform::log`]. The ring is what lets a test assert **structurally** — that a call
//! produced a record with this priority, this tag and this message — rather than by scraping
//! stderr, which is the recording-mock shape this project's working agreements prefer over a
//! timing or an output-capture assertion.
//!
//! The ring is bounded and counts what it dropped, because "how much does the guest log" is a
//! number nobody has measured and an unbounded one is a guest-driven host allocation.
//!
//! # Priorities are matched exactly, never mapped to a neighbour
//!
//! A priority outside `android_LogPriority` is a refusal naming the value. It is the one argument
//! of these four that has a wrong answer available: mapping 42 to `ANDROID_LOG_UNKNOWN` would
//! record the line under a priority the guest did not ask for, and a guest passing 42 has either a
//! miscompiled call or a corrupted stack — both worth hearing about.
//!
//! `syslog`'s priority is `facility | severity` with the severity in the low three bits. The two
//! scales genuinely correspond, so the severity is *mapped* rather than guessed; the facility is
//! carried into the tag rather than dropped.

use omni_platform::log::{Priority, Record};

use crate::boundary::ImportCall;
use crate::error::AbiResult;
use crate::mem::Blame;

use super::view::GuestView;
use super::{active, enter, format};

/// `LOG_PRIMASK` from `<syslog.h>`: the low three bits of a `syslog` priority are the severity.
const SYSLOG_SEVERITY_MASK: i32 = 0x07;

/// One line the guest logged, as the instance's ring keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// The priority, already validated against the scale it came from.
    pub priority: Priority,
    /// The tag: `__android_log_print`'s, or `openlog`'s ident, or a fallback naming the call.
    pub tag: String,
    /// The formatted message, without a trailing newline.
    pub message: String,
}

/// Read a guest tag or ident as display text.
///
/// Lossy rather than a refusal, for the same reason `pthread_setname_np` is: a tag is a label the
/// guest chose, it is under no obligation to be UTF-8, and refusing a log line because of its
/// encoding would discard the line in order to complain about it.
fn read_label(view: &GuestView<'_>, pointer: u64, argument: usize) -> AbiResult<String> {
    if pointer == 0 {
        return Ok(String::new());
    }
    let at = omni_mem::GuestAddr::try_from(pointer)
        .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
    let bytes = view.mem().cstr(at, Blame::new(view.symbol(), view.address(), argument))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Turn the formatting core's one-char-per-byte result into display text.
///
/// The core works in Latin-1 because a guest format string is bytes rather than text;
/// [`format::to_bytes`] maps it back and **refuses** anything that is not one guest byte, so this
/// cannot silently substitute a character.
fn message_text(view: &GuestView<'_>, rendered: &str) -> AbiResult<String> {
    let bytes = format::to_bytes(view, rendered)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `int __android_log_print(int prio, const char *tag, const char *fmt, ...)`
///
/// Returns the number of bytes of the formatted message, which is what bionic's own
/// implementation returns and what nothing in practice reads.
pub(super) fn android_log_print(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (prio, tag_pointer, fmt, consumed, overflow) = {
        let mut a = c.args();
        let prio = a.next_i32()?;
        let tag = a.next_u64()?;
        let fmt = a.next_u64()?;
        // Taken from the cursor rather than counted by hand, exactly as `snprintf` does: the
        // split between named and variadic arguments cannot then be got wrong by miscounting.
        (prio, tag, fmt, a.consumed(), a.overflow())
    };
    let state = active(c.symbol(), c.address())?;
    let written = {
        let mut source = c.varargs(consumed, overflow, 3);
        let view = enter(c, &state);
        let Some(priority) = Priority::from_android(prio) else {
            return Err(view.refusal(format!(
                "the guest logged at priority {prio}, which is not an `android_LogPriority` \
                 (0 UNKNOWN through 8 SILENT). Recording the line under a priority the guest did \
                 not ask for would hide a miscompiled call or a corrupted stack"
            )));
        };
        let tag = read_label(view.blaming(1), tag_pointer, 1)?;
        let rendered = format::render(&view, fmt, 2, &mut source)?;
        let message = message_text(&view, &rendered)?;
        let length = i32::try_from(message.len())
            .map_err(|_| view.refusal("a log message longer than an int can report"))?;
        state.bionic.log(LogRecord { priority, tag, message });
        length
    };
    c.ret().i32(written);
    Ok(())
}

/// `void syslog(int priority, const char *fmt, ...)`
///
/// `priority` is `facility | severity`. The severity maps onto the Android scale — the two are
/// both "how bad is this" — and the facility goes into the tag rather than being dropped, beside
/// whatever `openlog` last set as the ident.
pub(super) fn syslog(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (priority, fmt, consumed, overflow) = {
        let mut a = c.args();
        let priority = a.next_i32()?;
        let fmt = a.next_u64()?;
        (priority, fmt, a.consumed(), a.overflow())
    };
    let state = active(c.symbol(), c.address())?;
    {
        let mut source = c.varargs(consumed, overflow, 2);
        let view = enter(c, &state);
        let severity = priority & SYSLOG_SEVERITY_MASK;
        let Some(mapped) = Priority::from_syslog_severity(severity) else {
            // Unreachable: a three-bit mask cannot produce a value outside 0..=7. Written as a
            // refusal rather than an `expect` because a panic reachable from a guest argument is
            // Critical whether or not the argument can actually reach it.
            return Err(view.refusal(format!(
                "the guest called syslog with priority {priority}, whose severity {severity} is \
                 not one of the eight syslog levels"
            )));
        };
        let facility = priority & !SYSLOG_SEVERITY_MASK;
        let ident = state.bionic.syslog_ident();
        let tag = match (ident, facility) {
            (Some(ident), 0) => ident,
            (Some(ident), facility) => format!("{ident}[facility {facility}]"),
            (None, 0) => "syslog".to_string(),
            (None, facility) => format!("syslog[facility {facility}]"),
        };
        let rendered = format::render(&view, fmt, 1, &mut source)?;
        let message = message_text(&view, &rendered)?;
        state.bionic.log(LogRecord { priority: mapped, tag, message });
    }
    c.ret().void();
    Ok(())
}

/// `void openlog(const char *ident, int option, int facility)`
///
/// Records the ident, which is what later `syslog` lines are tagged with. `option` (`LOG_PID`,
/// `LOG_CONS`, …) and the default `facility` are read and **not** acted on, and that is stated
/// rather than silent: every one of those options is about where a real `syslogd` puts the line
/// and how it decorates it, and there is no `syslogd`. Nothing the guest can observe depends on
/// them, because `openlog` returns `void` and `syslog` returns `void`.
///
/// A null ident clears it, which is what C says it means: the ident reverts to the program name.
pub(super) fn openlog(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (ident_pointer, option, facility) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_i32()?, a.next_i32()?)
    };
    let _ = (option, facility);
    let state = active(c.symbol(), c.address())?;
    {
        let view = enter(c, &state);
        let ident = if ident_pointer == 0 {
            None
        } else {
            Some(read_label(view.blaming(0), ident_pointer, 0)?)
        };
        state.bionic.set_syslog_ident(ident);
    }
    c.ret().void();
    Ok(())
}

/// `void closelog(void)`
///
/// Clears the ident. There is no descriptor to close — that is the part of `closelog` that needs a
/// `syslogd` — so what is left is exactly this, and it is the half a guest can observe through a
/// subsequent `syslog` line's tag.
pub(super) fn closelog(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let state = active(c.symbol(), c.address())?;
    state.bionic.set_syslog_ident(None);
    c.ret().void();
    Ok(())
}

/// Send a record to the host's standard error.
///
/// Separated from the ring so that a test can keep the ring and silence the stream.
pub(crate) fn emit(record: &LogRecord) {
    omni_platform::log::emit(&Record {
        priority: record.priority,
        tag: &record.tag,
        message: &record.message,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The severity mask is the low three bits, and every severity maps.
    #[test]
    fn the_syslog_severity_is_the_low_three_bits() {
        assert_eq!(SYSLOG_SEVERITY_MASK, 0x07);
        // LOG_USER (1 << 3) | LOG_WARNING (4) is the classic shape.
        let priority = (1 << 3) | 4;
        assert_eq!(priority & SYSLOG_SEVERITY_MASK, 4);
        assert_eq!(priority & !SYSLOG_SEVERITY_MASK, 8);
        for severity in 0..8 {
            assert!(
                Priority::from_syslog_severity(severity).is_some(),
                "severity {severity} must map"
            );
        }
    }
}
