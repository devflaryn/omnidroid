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
//! # The ring bounds records **and** bytes, and says which one it gave up
//!
//! [`LogRing`] is the fix for adapter-review finding M3, which was that the ring bounded the
//! *number* of records and nothing else. The guest chooses the length of every message, so a
//! count is not a bound on memory: 256 records of a megabyte each is a guest-driven host
//! allocation with `log_dropped()` still reporting zero. Two bounds now, and they are
//! independent on purpose:
//!
//! | bound | number | why it is that number |
//! |---|---|---|
//! | records | [`LOG_CAPTURE_MAX`](super::LOG_CAPTURE_MAX) = 256 | unchanged policy: how much the engine logs has still not been measured |
//! | bytes | [`LOG_CAPTURE_MAX_BYTES`] = 256 KiB | policy, chosen **below** the product of the other two so that it binds rather than restating them — see its own note |
//!
//! And the ring keeps the two outcomes apart, because they are different failures and a host
//! reading the ring has to be able to tell them apart:
//!
//! * **dropped** — a whole record was evicted, oldest first, to make room.
//!   [`Bionic::log_dropped`](super::Bionic::log_dropped) counts them. The line is gone.
//! * **truncated** — a record was *shortened* on the way in, by the platform's own caps.
//!   [`Bionic::log_truncated`](super::Bionic::log_truncated) counts them, **and** the record
//!   itself carries [`Truncation`] with the byte counts it had before, **and** the rendered
//!   stderr line carries [`omni_platform::log::TRUNCATION_MARKER`]. The line is there and is
//!   short, and a reader can see that at all three levels.
//!
//! A single counter for both would have been the believable wrong answer: "the ring lost 40
//! things" reads the same whether forty lines vanished or forty lines lost their tails, and the
//! remedies are opposite ones.
//!
//! # `__android_log_print` truncates where a device truncates — and refuses where it must
//!
//! Adapter-review finding M4: this handler refused on paths where the platform has a defined
//! behaviour, and a refusal aborts the whole run. The rule each path was decided on is *a refusal
//! is right when this layer would otherwise have to invent an answer, and wrong when the platform
//! has a defined behaviour this layer can reproduce.*
//!
//! An over-long message is the second case and is now reproduced rather than refused:
//! [`omni_platform::log::liblog_caps`] carries the two AOSP constants and the order they apply
//! in. Nothing about that is a stub — it is what the device does — but a truncation nobody can
//! see would be worse than a refusal, so it is reported three ways (above).
//!
//! **Finding W1 was the same defect one layer down**, and is closed the same way. `liblog` does
//! not cap a *formatted* message after the fact: it formats into `char buf[LOG_BUF_SIZE]` in the
//! first place, so the conversion that overruns the buffer is cut inside `vsnprintf` and the
//! ones after it produce nothing. Capping afterwards needs the formatter to have produced the
//! whole thing first, which is what made a 1 MiB result (`OutputTooLarge`) and a 64 KiB field
//! (`FieldTooWide`) into refusals — aborting a guest over a log line, which is the outcome the
//! first paragraph of this file says the module exists to prevent.
//!
//! So the destination size goes *in*: [`format::render_bounded`](super::format::render_bounded)
//! is given [`FORMAT_BUDGET`] and keeps the prefix that fits, and the count of what it could
//! not keep comes back beside it so that `capped_record_of` can report a cut the length
//! comparison in [`capped_record`] can no longer see.
//!
//! The paths that remain refusals are the first case, and each one's doc comment below says what
//! this layer would otherwise have had to invent. The inventory is in
//! [`android_log_print`]'s own documentation, path by path, because a count of refusals is not a
//! statement of which paths refuse (`VERIFICATION.md` entry 1).
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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use omni_platform::log::{
    liblog_caps, Kept, Priority, Record, Truncation, MAX_MESSAGE_BYTES, MAX_TAG_AND_MESSAGE_BYTES,
};

use crate::boundary::ImportCall;
use crate::error::AbiResult;
use crate::mem::Blame;

use super::view::GuestView;
use super::{active, enter, format, LOG_CAPTURE_MAX};

/// `LOG_PRIMASK` from `<syslog.h>`: the low three bits of a `syslog` priority are the severity.
const SYSLOG_SEVERITY_MASK: i32 = 0x07;

/// The destination size the formatter is given for a log line: **1,023**.
///
/// **Not a policy number of this layer's own.** `liblog`'s `__android_log_print` formats with
/// `vsnprintf(buf, LOG_BUF_SIZE, fmt, ap)` into `char buf[LOG_BUF_SIZE]`, so the formatter's
/// destination is `LOG_BUF_SIZE` less the NUL — which is what [`MAX_MESSAGE_BYTES`] is, beside
/// the AOSP file it was read out of. Written as a name rather than passed as a literal at the two
/// call sites so that `the_format_budget_is_liblogs_own_buffer` can assert the relation, which is
/// the only kind of test a constant has (`VERIFICATION.md` entry 12).
///
/// Giving the formatter a *larger* budget would not be wrong so much as pointless — the payload
/// cap would cut the excess a moment later — but it would make the two cuts report different
/// numbers for the same loss. A smaller one would shorten lines a device carries whole.
const FORMAT_BUDGET: usize = MAX_MESSAGE_BYTES;

/// One line the guest logged, as the instance's ring keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// The priority, already validated against the scale it came from.
    pub priority: Priority,
    /// The tag: `__android_log_print`'s, or `openlog`'s ident, or a fallback naming the call.
    pub tag: String,
    /// The formatted message, without a trailing newline.
    pub message: String,
    /// What the platform's caps removed on the way in, or `None` for a record that arrived whole.
    ///
    /// **This is the half of finding M3 that is not a bound.** A ring that shortens a record and
    /// does not say so hands its reader a line that looks complete, and no amount of counting
    /// tells them otherwise. `tag` and `message` are what survived; this says what they were.
    pub truncated: Option<Truncation>,
}

/// The most host bytes one record can occupy, given the platform's caps.
///
/// [`MAX_TAG_AND_MESSAGE_BYTES`] is 4,065 **guest** bytes, and a guest byte that is not valid
/// UTF-8 becomes one three-byte `U+FFFD` in the host `String` — so the host side is up to three
/// times the guest side, and the naive figure of 4,065 would have been a believable wrong answer
/// exactly 8,130 bytes per record too small. The `String`s are shrunk to fit as they are built —
/// see this module's private `lossy` — so that this is a real ceiling on the allocation and not
/// just on the length.
pub const RECORD_MAX_FOOTPRINT: usize =
    core::mem::size_of::<LogRecord>() + 3 * MAX_TAG_AND_MESSAGE_BYTES;

/// How many host bytes of log records one instance keeps.
///
/// **A policy number, and the second of the ring's two bounds.** The record cap alone permits
/// [`LOG_CAPTURE_MAX`] × [`RECORD_MAX_FOOTPRINT`] ≈ 3.0 MiB per instance, which is the figure
/// finding M3 is about; this is set to roughly a twelfth of it so that it is a **bound and not a
/// restatement** — a byte cap at or above that product would be a check no input can fail, which
/// `VERIFICATION.md` entry 12 says is not a check at all.
/// `the_two_ring_bounds_are_both_live` asserts that relation between the constants rather than
/// trusting this paragraph.
///
/// What it costs: all 256 records still fit whenever a record's tag and message together are at
/// most 944 host bytes (256 KiB / 256, less the struct), which is 92% of `liblog`'s own
/// 1,023-byte message cap. A run that logs longer lines than that keeps fewer of them and
/// **counts every one it dropped**.
pub const LOG_CAPTURE_MAX_BYTES: usize = 256 * 1024;

/// One record must always fit the byte bound on its own, or [`LogRing::push`] could empty the
/// ring and still not have room — which would be a ring that drops everything and holds nothing,
/// with every counter looking plausible. A compile-time assertion rather than a test, because it
/// is a relation between two constants that no input can make false at run time
/// (`VERIFICATION.md` entry 12); the *other* direction, that the byte bound is small enough to
/// bind at all, is `the_two_ring_bounds_are_both_live`.
const _: () = assert!(LOG_CAPTURE_MAX_BYTES >= RECORD_MAX_FOOTPRINT);

/// The instance's bounded capture ring.
///
/// Lives here rather than as three fields on [`Bionic`](super::Bionic) because the two bounds and
/// the two counters have to move together: an eviction has to adjust the byte total in the same
/// critical section that removed the record, and a byte total maintained next to a `VecDeque`
/// somebody else can `pop_front` is a total that goes wrong silently.
#[derive(Debug, Default)]
pub struct LogRing {
    /// The records and their running byte total, under one lock.
    state: Mutex<RingState>,
    /// Whole records evicted to make room, by either bound.
    dropped: AtomicU64,
    /// Host bytes those evicted records were holding, so "the ring dropped 44" can be read as a
    /// quantity of log and not only as a count of lines.
    dropped_bytes: AtomicU64,
    /// Records admitted already shortened — carrying a [`Truncation`]. **Never an eviction**: the
    /// two are different failures with opposite remedies, and one counter for both would say
    /// neither.
    truncated: AtomicU64,
}

#[derive(Debug, Default)]
struct RingState {
    records: VecDeque<LogRecord>,
    /// The sum of [`footprint`] over `records`, maintained by every push and every eviction.
    bytes: usize,
}

/// The host bytes one record occupies: the struct, plus both `String` allocations.
///
/// `capacity`, not `len`: the allocation is what the bound is about, and a `String` whose buffer
/// is larger than its contents still holds the buffer. [`lossy`] shrinks every string it builds,
/// so in practice the two are equal — measuring `capacity` means the bound stays honest if that
/// ever stops being true.
fn footprint(record: &LogRecord) -> usize {
    core::mem::size_of::<LogRecord>()
        .saturating_add(record.tag.capacity())
        .saturating_add(record.message.capacity())
}

impl LogRing {
    /// An empty ring.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit one record, evicting oldest-first until it fits **both** bounds.
    ///
    /// # Arithmetic
    ///
    /// The byte total is a sum over guest-controlled lengths and `cargo test --workspace
    /// --release` runs where `+` wraps silently (`VERIFICATION.md` entry 3), so both directions
    /// saturate. Saturation is not a substituted answer here: a saturated total compares greater
    /// than the cap, which evicts, which is the same thing the true total would have done.
    pub fn push(&self, record: LogRecord) {
        if record.truncated.is_some() {
            self.truncated.fetch_add(1, Ordering::Relaxed);
        }
        let cost = footprint(&record);
        let mut state = self.state.lock();
        loop {
            let over_records = state.records.len() >= LOG_CAPTURE_MAX;
            let over_bytes = state.bytes.saturating_add(cost) > LOG_CAPTURE_MAX_BYTES;
            if !(over_records || over_bytes) {
                break;
            }
            // `else break` rather than an `expect`: an empty ring that still does not fit would
            // mean a record larger than the whole byte bound, which `RECORD_MAX_FOOTPRINT`
            // makes impossible -- but a panic reachable from a guest-supplied length is a
            // Critical whether or not the length can actually reach it.
            let Some(gone) = state.records.pop_front() else { break };
            state.bytes = state.bytes.saturating_sub(footprint(&gone));
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.dropped_bytes.fetch_add(footprint(&gone) as u64, Ordering::Relaxed);
        }
        state.bytes = state.bytes.saturating_add(cost);
        state.records.push_back(record);
    }

    /// Every record still in the ring, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<LogRecord> {
        self.state.lock().iter_cloned()
    }

    /// How many whole records the ring evicted, by either bound.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// How many host bytes those evicted records held.
    #[must_use]
    pub fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes.load(Ordering::Relaxed)
    }

    /// How many records were **shortened** on the way in. Distinct from [`LogRing::dropped`].
    #[must_use]
    pub fn truncated(&self) -> u64 {
        self.truncated.load(Ordering::Relaxed)
    }

    /// How many host bytes the ring is holding right now.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.state.lock().bytes
    }
}

impl RingState {
    fn iter_cloned(&self) -> Vec<LogRecord> {
        self.records.iter().cloned().collect()
    }
}

/// Turn guest bytes into display text with no slack in the allocation.
///
/// Lossy rather than a refusal, for the same reason `pthread_setname_np` is: a tag and a message
/// are labels the guest chose, they are under no obligation to be UTF-8, and refusing a log line
/// because of its encoding would discard the line in order to complain about it.
///
/// `shrink_to_fit` is what makes [`footprint`] a real bound: the lossy conversion of invalid
/// bytes builds a `String` that grows by doubling, so its buffer can be twice the text it holds.
/// For the common case — valid UTF-8 — the conversion borrows and the shrink is a no-op.
fn lossy(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    text.shrink_to_fit();
    text
}

/// Read a guest tag or ident as bytes.
///
/// Bytes rather than text because the platform's caps are counted in **guest** bytes: capping the
/// host `String` would cap a different thing, by up to a factor of three, on exactly the input
/// (not-UTF-8) a hostile guest picks.
///
/// # Errors
///
/// The two refusals here are paths **T2** and **T3** of [`android_log_print`]'s inventory.
fn read_label_bytes(view: &GuestView<'_>, pointer: u64, argument: usize) -> AbiResult<Vec<u8>> {
    if pointer == 0 {
        return Ok(Vec::new());
    }
    let at = omni_mem::GuestAddr::try_from(pointer)
        .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
    view.mem().cstr(at, Blame::new(view.symbol(), view.address(), argument))
}

/// Read a guest tag or ident as display text. See [`read_label_bytes`] and [`lossy`].
fn read_label(view: &GuestView<'_>, pointer: u64, argument: usize) -> AbiResult<String> {
    Ok(lossy(&read_label_bytes(view, pointer, argument)?))
}

/// Turn the formatting core's one-char-per-byte result into the guest's bytes.
///
/// The core works in Latin-1 because a guest format string is bytes rather than text;
/// [`format::to_bytes`] maps it back and **refuses** anything that is not one guest byte, so this
/// cannot silently substitute a character.
fn message_bytes(view: &GuestView<'_>, rendered: &str) -> AbiResult<Vec<u8>> {
    format::to_bytes(view, rendered)
}

/// Build one record with the platform's caps applied, reporting what they took.
///
/// The cut is made on the **byte** vectors, before the lossy conversion, for two reasons. It is
/// where the device makes it, so the surviving bytes are the surviving bytes. And it means no
/// `String::truncate` on a host string — that call panics on a byte index that is not a character
/// boundary, and the index here comes from a guest-chosen length, which would have made a
/// one-line panic reachable from `__android_log_print("%s", "é…")`.
fn capped_record(priority: Priority, tag: &[u8], message: &[u8]) -> (LogRecord, Kept) {
    let kept = liblog_caps(tag.len(), message.len());
    let truncated = if kept.tag < tag.len() || kept.message < message.len() {
        Some(Truncation { tag_bytes: tag.len(), message_bytes: message.len() })
    } else {
        None
    };
    let record = LogRecord {
        priority,
        tag: lossy(&tag[..kept.tag]),
        message: lossy(&message[..kept.message]),
        truncated,
    };
    (record, kept)
}

/// [`capped_record`], for a message the **formatter** already cut.
///
/// `full_message_bytes` is what `vsnprintf` would have returned: the length of the whole
/// formatted message, which is not `message.len()` once
/// [`format::render_bounded`](super::format::render_bounded) has stopped at `liblog`'s
/// 1,024-byte buffer. [`capped_record`] compares `message.len()` against the caps and therefore
/// sees nothing to report for a message that arrived already at the cap — a **silent**
/// truncation, which this module's header says is worse than a refusal and which this restores
/// the report for.
///
/// The three reporting channels are unchanged: the counter, the record's own [`Truncation`], and
/// the marker on the rendered stderr line.
fn capped_record_of(
    priority: Priority,
    tag: &[u8],
    message: &[u8],
    full_message_bytes: usize,
) -> (LogRecord, Kept) {
    let (mut record, kept) = capped_record(priority, tag, message);
    if full_message_bytes > message.len() {
        record.truncated =
            Some(Truncation { tag_bytes: tag.len(), message_bytes: full_message_bytes });
    }
    (record, kept)
}

/// `int __android_log_print(int prio, const char *tag, const char *fmt, ...)`
///
/// Returns the number of bytes of the formatted message this layer accepted — that is, the
/// message's length **after** the platform's caps, counted in guest bytes.
///
/// **A divergence from AOSP `main`, recorded rather than hidden.** Today's
/// `__android_log_print` returns a literal `1` on success and `-EPERM` when
/// `__android_log_is_loggable` says the line is filtered out; the byte count is what older
/// `liblog` returned, by passing `__android_log_write`'s result through. Nothing in the reachable
/// set reads the value at all, so this is not a wrong answer a guest can act on, and the byte
/// count is the more useful of the two here. The filtering is **not** modelled: there is no
/// `persist.log.tag` property store, so `-EPERM` would be a guess about a device configuration
/// that does not exist.
///
/// # Every path this handler refuses on
///
/// Finding M4 said "at least eight, six undiscussed". The real number, from walking every `?` and
/// every `Err` in this function and in everything it calls, is **eighteen**: thirteen that an
/// input can reach, of which twelve are reachable from guest-supplied values and one is host
/// state, and **five that no input can reach**, which is itself worth knowing — a branch no input
/// can take is not a check (`VERIFICATION.md` entry 12). They are named rather than counted,
/// because a count cannot see a substitution (entry 1).
///
/// The table below has nineteen rows because **W1** is kept in it as a row that is no longer a
/// refusal: a defect that has been closed is worth more in the inventory than out of it, and the
/// split of W1 into the half that truncates and the half (**W1b**) that still refuses is exactly
/// the distinction the finding asked for. The eighteen are the other rows.
///
/// | id | path | decision |
/// |---|---|---|
/// | **A1** | no live instance for this call ([`active`]) | **kept.** Host state rather than guest input; there is no instance to log *to*, so there is nothing to invent |
/// | **A2** | a named argument is a stack argument and the stack is unmapped | **kept, and no input reaches it**: all three named arguments live in `X0`-`X2`, so `Args` never touches the stack for this symbol |
/// | **P1** | `prio` is not an `android_LogPriority` | **kept.** Would have to invent which of the nine priorities to file the line under. Note the device does *not* refuse — it writes the byte through — but reproducing that needs a [`Priority`] that can hold 42, and answering with a neighbour is the wrong answer the module header is about |
/// | **T1** | the tag pointer is wider than the host's `usize` | **kept, and no input reaches it** on any host this builds for (`usize` is 64 bits): a portability guard, labelled as one rather than left to read as a live check |
/// | **T2** | the tag pointer is not readable | **kept.** Would have to invent the tag's bytes. A real device faults inside `strlen` here, so there is no platform behaviour to copy |
/// | **T3** | the tag has no NUL inside its region or inside `GuestMem::STRING_LIMIT` | **kept.** Would have to invent where the tag ends |
/// | **F1** | the format string is null | **kept.** Would have to invent a message. Printing `(null)` is glibc's behaviour, not bionic's, so copying it would be copying the wrong platform |
/// | **F2** | the format pointer is wider than the host's `usize` | **kept**, and no input reaches it, as **T1** |
/// | **F3** | the format pointer is not readable | **kept.** Would have to invent the format string |
/// | **F4** | the format string has no NUL | **kept.** Would have to invent where it ends |
/// | **F5** | the format string contains `%n` | **kept**, and it is what the device does: bionic's `vfprintf.cpp` answers `%n` with `__fortify_fatal("%%n not allowed on Android")` |
/// | **F6** | `%Lf` / `%ls` / `%lc` | **kept.** Would have to invent 128-bit long-double digits, or a `wchar_t` encoding and a locale, none of which exists in the formatting core |
/// | **F7** | an unknown or malformed conversion, refused by `plan` before any argument is fetched | **kept, and this is the one worth spelling out.** The damage is not the one field: an unknown conversion means the planner cannot know whether it consumes an argument, so *every later conversion reads a different argument*. What would be invented is the whole rest of the line, not a field |
/// | **V1** | a variadic argument lies past the mapped overflow area, or the overflow pointer would leave the address space | **kept.** This is a format string claiming more arguments than the caller passed; would have to invent the value |
/// | **S1** | a `%s` argument's pointer is wider than `usize`, unreadable, or unterminated | **kept**, for the reasons given for **T1**-**T3**. A **null** `%s` is not in this list: it prints `(null)`, which is what bionic prints |
/// | **W1** | the formatted output passes `MAX_OUTPUT` (1 MiB), or one field's width passes `MAX_FIELD_WIDTH` (64 KiB) | **gone — now a truncation.** The formatter is given `liblog`'s own destination size and keeps the prefix that fits, which is what `vsnprintf(buf, LOG_BUF_SIZE, …)` does. See below |
/// | **W1b** | one field's **precision** passes `MAX_FIELD_WIDTH` on `%e %E %g %G %a %A` | **kept, and it is the one arm that could not be made byte-correct.** See below |
/// | **W2** | a format-time argument-kind mismatch (`'*' width needs an int`, `%s needs a string`, `%p needs a pointer`) | **kept, and no input reaches it**: `plan` and `format` walk the same format string, so the kind fetched is always the kind expected. A planner and a formatter that could disagree is the defect this would catch |
/// | **M1** | the formatted result contains a character above `U+00FF` | **kept, and no input reaches it**: the core copies guest bytes as Latin-1 and every conversion it produces is ASCII. Refused rather than substituted, because a substituted byte is a believable wrong answer in a string a guest will read |
///
/// **Two refusals are gone**, replaced by the platform's own behaviour: an over-long formatted
/// message and an over-long tag, now cut by [`liblog_caps`] exactly as `liblog` cuts them. A
/// third, `i32::try_from(message.len())` — "a log message longer than an int can report" — was a
/// branch no input could take even before the cap, and is **deleted** rather than kept as
/// reassurance (entry 12); what replaces it is a `const` assertion on the cap.
///
/// ## The example M4 gives does not happen the way M4 says
///
/// M4's illustration is "a >1 MiB `%s` aborts the whole run". It does abort, and **not through
/// the path M4 names**: `GuestMem::cstr` walks at most `STRING_LIMIT` = 64 KiB, so a one-megabyte
/// `%s` is refused as **S1** (unterminated) long before the formatter has produced anything to be
/// too large. The over-long-output path (**W1**) needs a format string that *builds* a megabyte,
/// such as seventeen 64 KiB `%s` arguments or a wide field. The distinction matters because the
/// two want different fixes, and a fix aimed at the stated mechanism would have closed neither.
/// (`VERIFICATION.md` entry 10: read the source, not the summary of it.)
///
/// ## **W1**, closed: the order the pieces go in is the whole of it
///
/// The budget is [`MAX_MESSAGE_BYTES`], which is `LOG_BUF_SIZE` less the NUL — the same
/// destination size `liblog` gives `vsnprintf`, so the same bytes. What made the arm need
/// thought rather than a `match` is that a partial field is **not** a shortened field:
///
/// * `%70000d` right-justified is 69,998 spaces and then `42`, so a device's buffer holds
///   **1,023 spaces** and no digits at all. Clamping the width to the budget — the obvious fix —
///   produces 1,021 spaces and then `42`: the right length, the right characters, the wrong
///   order, and a tail a device does not have.
/// * `%-70000d` is the body first, so `42` and then spaces.
/// * `%070000d` of -42 is the **sign** first, so `-` and then zeros.
///
/// All three are exact, because the padding is emitted as a counted fill rather than built:
/// nothing has to clamp a guest-chosen width to anything. The same holds for a precision, which
/// is a run of zeros at a known offset for the integer conversions, a no-op for `%s`, and a run
/// of zeros past `EXACT_FRACTION_DIGITS` for `%f` — a finite `double`'s exact decimal expansion
/// is at most 1,074 fraction digits long, so a longer precision appends zeros and rounds
/// nothing.
///
/// ## **W1b**: the arm that is not byte-correct, refused by name
///
/// `%.70000e`, and the same for `E g G a A`. Their digits are built through
/// `10u64.pow(precision.min(15))` in `format_exp`, and `format_g` delegates to a precision it
/// derives, so past fifteen places what this engine produces is **already** not `vsnprintf`'s —
/// a divergence that predates the budget. Under a budget those wrong digits would land in the
/// *visible* prefix rather than being discarded with the rest, so the refusal is kept and names
/// the conversion. Their **width** is a separate question and is honoured, because padding is
/// placeable whatever the body is.
///
/// ## Still refusing where the device truncates: **S1**/**T3** in part
///
/// A `%s` argument longer than `GuestMem::STRING_LIMIT` (64 KiB) is refused as unterminated
/// where `vsnprintf` would have copied 1,023 bytes of it and never looked for the NUL. Closing
/// that needs a **bounded** `cstr` read in `mem.rs` — the budget cannot help, because the bytes
/// never reach the formatter — and the same is true of an over-long tag. Recorded here, at the
/// place it bites, rather than only in a review document.
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
        let tag = read_label_bytes(view.blaming(1), tag_pointer, 1)?;
        // `MAX_MESSAGE_BYTES`, because that is `LOG_BUF_SIZE` less the NUL and `liblog` formats
        // with `vsnprintf(buf, LOG_BUF_SIZE, fmt, ap)`: the same destination size, so the same
        // bytes. Not a policy number of this layer's own -- `omni_platform::log` names the AOSP
        // file it was read out of.
        let rendered = format::render_bounded(&view, fmt, 2, &mut source, FORMAT_BUDGET)?;
        let message = message_bytes(&view, &rendered.text)?;
        let (record, kept) = capped_record_of(priority, &tag, &message, rendered.full);
        state.bionic.log(record);
        // At most `MAX_MESSAGE_BYTES` (1,023) by the cap above, so the narrowing cannot lose a
        // bit. Written as a static assertion on the constant rather than as a runtime check,
        // because the runtime check would be a branch no input can take.
        const _: () = assert!(omni_platform::log::MAX_MESSAGE_BYTES < i32::MAX as usize);
        kept.message as i32
    };
    c.ret().i32(written);
    Ok(())
}

/// `void syslog(int priority, const char *fmt, ...)`
///
/// `priority` is `facility | severity`. The severity maps onto the Android scale — the two are
/// both "how bad is this" — and the facility goes into the tag rather than being dropped, beside
/// whatever `openlog` last set as the ident.
///
/// The same caps apply as to [`android_log_print`], and that is the device's arrangement rather
/// than a convenience: bionic's `syslog.cpp` implements `vsyslog` by calling
/// `__android_log_vprint`, whose body is `__android_log_print`'s — the same
/// `char buf[LOG_BUF_SIZE]` and the same `vsnprintf`. A `syslog` that was capped differently
/// here would be a divergence invented by this layer.
///
/// Its refusal inventory is [`android_log_print`]'s less **P1** (the severity is masked to three
/// bits, so it always maps) and shifted by one argument.
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
        // bionic's `vsyslog` calls `__android_log_vprint`, whose body is `__android_log_print`'s
        // -- the same `char buf[LOG_BUF_SIZE]` -- so the budget is the same one, and a `syslog`
        // budgeted differently here would be a divergence invented by this layer.
        let rendered = format::render_bounded(&view, fmt, 1, &mut source, FORMAT_BUDGET)?;
        let message = message_bytes(&view, &rendered.text)?;
        let (record, _) = capped_record_of(mapped, tag.as_bytes(), &message, rendered.full);
        state.bionic.log(record);
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
///
/// The ident is **not** capped here, and the asymmetry with [`syslog`]'s tag is deliberate: the
/// ident is one string of host state per instance, bounded by `GuestMem::STRING_LIMIT` at 64 KiB
/// and overwritten by the next `openlog`, not a per-record allocation a guest can drive in a
/// loop. It is capped where it becomes a record's tag, which is where the device caps it.
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
/// Separated from the ring so that a test can keep the ring and silence the stream. The
/// truncation travels with the record, so a run watched only through stderr still sees that a
/// line lost its tail.
pub(crate) fn emit(record: &LogRecord) {
    omni_platform::log::emit(&Record {
        priority: record.priority,
        tag: &record.tag,
        message: &record.message,
        truncated: record.truncated,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_platform::log::{format_line, TRUNCATION_MARKER};

    /// A record with a message of `bytes` 'x' and a tag of `tag` 't', pre-cap.
    fn made(tag: usize, message: usize) -> LogRecord {
        capped_record(Priority::Info, &vec![b't'; tag], &vec![b'x'; message]).0
    }

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

    /// **Both of the ring's bounds are live, asserted as a relation between the constants.**
    ///
    /// This is the detector for the shape `VERIFICATION.md` entry 12 describes: a byte cap set at
    /// or above `LOG_CAPTURE_MAX * RECORD_MAX_FOOTPRINT` is not a second bound, it is the first
    /// one restated, and every behavioural test would still pass. A structural assertion between
    /// two constants catches it where no input can.
    #[test]
    fn the_two_ring_bounds_are_both_live() {
        // That a single record always fits is asserted at compile time, beside the constant.
        // This is the other direction: the byte cap is strictly below what the record cap alone
        // would permit, so it is capable of binding at all.
        let product = LOG_CAPTURE_MAX * RECORD_MAX_FOOTPRINT;
        assert!(
            LOG_CAPTURE_MAX_BYTES < product,
            "the byte bound {LOG_CAPTURE_MAX_BYTES} does not bind: the record bound alone \
             already permits only {product}"
        );
        // The figures the doc comment states, so a moved constant fails here and not silently.
        assert_eq!(LOG_CAPTURE_MAX, 256);
        assert_eq!(LOG_CAPTURE_MAX_BYTES, 262_144);
        assert_eq!(RECORD_MAX_FOOTPRINT, core::mem::size_of::<LogRecord>() + 12_195);
        // ~3.0 MiB per instance is what the record bound alone still permits, which is the
        // figure M3 is about -- smaller than M3's 0.8 GiB only because the platform's own caps
        // now bound each record, and still far more than a ring for inspection needs.
        assert!(product >= 3_000_000, "{product} bytes, and the doc comment says ~3.0 MiB");
        assert!(
            LOG_CAPTURE_MAX_BYTES * 11 < product && product < LOG_CAPTURE_MAX_BYTES * 13,
            "the doc comment says roughly a twelfth; it is {}",
            product / LOG_CAPTURE_MAX_BYTES
        );
    }

    /// **The byte bound evicts where the record bound would not have**, and says so.
    ///
    /// The whole of finding M3 in one assertion. n = 100 pushes of a maximum-size record, which
    /// is well under [`LOG_CAPTURE_MAX`] — so a ring that bounds only records keeps all hundred
    /// and reports zero dropped, which is exactly what the old ring did.
    ///
    /// The size comes from the **tag**, not the message: `liblog`'s `vsnprintf` cap puts a
    /// message at 1,023 bytes whatever else happens, and the payload cap is what lets a tag reach
    /// 4,065. A test built on a long message would have been a test of nothing, because the
    /// record it pushed would have been a quarter of the size it looked.
    #[test]
    fn the_ring_bounds_bytes_where_the_record_count_would_not_have() {
        let ring = LogRing::new();
        for _ in 0..100 {
            ring.push(made(MAX_TAG_AND_MESSAGE_BYTES, 8));
        }
        let held = ring.records().len();
        assert!(
            held < 100,
            "a ring that bounded only records would have kept all 100; it kept {held}"
        );
        assert!(held < LOG_CAPTURE_MAX, "the record bound was never reached: {held} records");
        assert!(ring.dropped() > 0, "and the eviction must be counted, not silent");
        assert_eq!(
            ring.dropped() as usize + held,
            100,
            "every pushed record is either held or counted as dropped"
        );
        assert!(
            ring.bytes() <= LOG_CAPTURE_MAX_BYTES,
            "{} bytes held, cap {LOG_CAPTURE_MAX_BYTES}",
            ring.bytes()
        );
        assert!(ring.dropped_bytes() > 0, "the bytes the evictions freed are reported too");
    }

    /// **The record bound still binds for small records**, and the byte bound does not fire.
    ///
    /// The companion to the test above: with short lines the ring must behave exactly as it did
    /// before M3, because a byte bound that also shortened the ordinary case would be a
    /// regression dressed as a fix. n = 300 pushes of a 7-byte message.
    #[test]
    fn the_record_bound_still_binds_for_ordinary_lines() {
        let ring = LogRing::new();
        for n in 0..300u32 {
            ring.push(LogRecord {
                priority: Priority::Info,
                tag: "loop".to_string(),
                message: format!("n={n}"),
                truncated: None,
            });
        }
        let records = ring.records();
        assert_eq!(records.len(), LOG_CAPTURE_MAX, "the record bound, unchanged");
        assert_eq!(ring.dropped(), 300 - LOG_CAPTURE_MAX as u64);
        assert_eq!(ring.truncated(), 0, "nothing here was shortened");
        // Membership at both ends, not just the count: a ring that evicted the newest instead of
        // the oldest keeps exactly the same number of records.
        assert_eq!(records[0].message, format!("n={}", 300 - LOG_CAPTURE_MAX as u32));
        assert_eq!(records[LOG_CAPTURE_MAX - 1].message, "n=299");
        assert!(
            ring.bytes() < LOG_CAPTURE_MAX_BYTES,
            "the byte bound must not have fired: {} bytes",
            ring.bytes()
        );
    }

    /// **"Dropped" and "truncated" are different numbers, and the injected fault moves only one.**
    ///
    /// `VERIFICATION.md` entry 11: a counter that rises under load but stays at zero under the
    /// injected fault is a watch, not a detector. So this asserts both directions — 300 short
    /// records move `dropped` and leave `truncated` at zero; one over-long record moves
    /// `truncated` and leaves `dropped` where it was.
    #[test]
    fn dropped_and_truncated_are_distinguishable_and_each_detects_only_its_own_fault() {
        let ring = LogRing::new();
        for n in 0..300u32 {
            ring.push(LogRecord {
                priority: Priority::Info,
                tag: "t".to_string(),
                message: format!("n={n}"),
                truncated: None,
            });
        }
        assert_eq!(ring.dropped(), 44, "300 - 256");
        assert_eq!(ring.truncated(), 0, "eviction must not read as shortening");

        let before = ring.dropped();
        ring.push(made(4, 1 << 20));
        assert_eq!(ring.truncated(), 1, "one record was shortened");
        assert_eq!(ring.dropped(), before + 1, "and it evicted exactly one to make room");

        // And the record itself says it, without reference to either counter.
        let last = ring.records().pop().expect("a record");
        let cut = last.truncated.expect("the record must carry its own truncation");
        assert_eq!(cut.message_bytes, 1 << 20, "what it was");
        assert_eq!(last.message.len(), MAX_MESSAGE_BYTES, "what survived");
        assert_eq!(cut.tag_bytes, 4);
        assert_eq!(last.tag.len(), 4, "a short tag is not touched");
    }

    /// **A record the caps did not touch carries no truncation**, at every boundary.
    ///
    /// The negative half of the test above: a `truncated` that is `Some` for a whole record says
    /// nothing when it is `Some` for a cut one. Asserted at exactly the cap and one past it,
    /// because an off-by-one there is the believable wrong answer.
    #[test]
    fn an_uncut_record_carries_no_truncation_and_the_boundary_is_exact() {
        assert_eq!(made(4, MAX_MESSAGE_BYTES).truncated, None, "exactly at the message cap");
        let past = made(4, MAX_MESSAGE_BYTES + 1);
        assert_eq!(
            past.truncated,
            Some(Truncation { tag_bytes: 4, message_bytes: MAX_MESSAGE_BYTES + 1 }),
            "one byte past it"
        );
        assert_eq!(past.message.len(), MAX_MESSAGE_BYTES);
        assert_eq!(made(0, 0).truncated, None, "an empty record");

        // The tag is cut at the payload cap, and the message loses the remainder with it.
        let fat = made(MAX_TAG_AND_MESSAGE_BYTES + 1, 10);
        assert_eq!(fat.tag.len(), MAX_TAG_AND_MESSAGE_BYTES);
        assert_eq!(fat.message.len(), 0, "the tag is iov[1] and is filled first");
        assert!(fat.truncated.is_some());
    }

    /// **A guest byte that is not UTF-8 cannot make one record exceed its footprint.**
    ///
    /// The hostile case the naive bound gets wrong: 4,065 bytes of `0x80` become 4,065
    /// three-byte replacement characters, so the host string is 12,195 bytes — three times the
    /// figure a reviewer counting guest bytes would have written down. n = one record at the
    /// worst input.
    #[test]
    fn a_record_of_invalid_utf8_still_fits_the_footprint_the_bound_is_built_on() {
        let record = capped_record(
            Priority::Info,
            &vec![0x80; MAX_TAG_AND_MESSAGE_BYTES],
            &vec![0x80; 1 << 20],
        )
        .0;
        assert_eq!(record.tag.len(), 3 * MAX_TAG_AND_MESSAGE_BYTES, "every byte widened");
        assert_eq!(record.message.len(), 0, "the tag took the whole payload");
        assert!(
            footprint(&record) <= RECORD_MAX_FOOTPRINT,
            "{} bytes against a stated ceiling of {RECORD_MAX_FOOTPRINT}",
            footprint(&record)
        );
        // And the shrink is what makes that true of the allocation, not only of the length.
        assert_eq!(record.tag.capacity(), record.tag.len(), "no slack in the buffer");

        // The same input through the ring: bounded, and every drop counted.
        let ring = LogRing::new();
        for _ in 0..64 {
            ring.push(capped_record(
                Priority::Info,
                &vec![0x80; MAX_TAG_AND_MESSAGE_BYTES],
                &[],
            ).0);
        }
        assert!(ring.bytes() <= LOG_CAPTURE_MAX_BYTES, "{} bytes held", ring.bytes());
        assert_eq!(ring.truncated(), 0, "nothing was cut: 4065 bytes is exactly the cap");
    }

    /// **A guest-chosen multi-byte character at the cut point does not panic.**
    ///
    /// The Critical this arrangement avoids: `String::truncate` panics on an index that is not a
    /// character boundary, and a cap applied to the host string would take that index straight
    /// from a guest-chosen length. Cutting the *bytes* makes the question not arise, and this is
    /// the regression test for it — a message of 'é' (two bytes) cut at the odd byte 1,023.
    #[test]
    fn a_cut_through_a_multibyte_character_cannot_panic() {
        let message: Vec<u8> = "é".repeat(4096).into_bytes();
        assert_eq!(message.len(), 8192, "two bytes each, so 1023 lands mid-character");
        let record = capped_record(Priority::Info, b"t", &message).0;
        assert_eq!(record.truncated.expect("cut").message_bytes, 8192);
        // 1,023 guest bytes: 511 whole 'é' plus one half, and the half becomes U+FFFD.
        assert!(record.message.ends_with('\u{FFFD}'), "{:?}", record.message);
        assert_eq!(record.message.chars().count(), 512);
    }

    /// **The budget handed to the formatter is `liblog`'s own buffer, asserted as a relation.**
    ///
    /// A constant has no behaviour to test, so what is testable is the relation it stands in
    /// (`VERIFICATION.md` entry 12, and the same shape as `the_two_ring_bounds_are_both_live`).
    /// Both directions matter and for different reasons: a *smaller* budget shortens lines a
    /// device carries whole, and a *larger* one leaves the payload cap to make a second cut that
    /// reports a different number for the same loss.
    #[test]
    fn the_format_budget_is_liblogs_own_buffer() {
        assert_eq!(FORMAT_BUDGET, MAX_MESSAGE_BYTES, "the formatter's destination is the buffer");
        assert_eq!(
            FORMAT_BUDGET,
            omni_platform::log::LOG_BUF_SIZE - 1,
            "`vsnprintf(buf, LOG_BUF_SIZE, ...)` writes LOG_BUF_SIZE - 1 characters and a NUL"
        );
        assert_eq!(FORMAT_BUDGET, 1023);
        // And the budget alone is never what cuts the *tag*: the payload cap does that, and the
        // two are independent numbers.
        assert!(FORMAT_BUDGET < MAX_TAG_AND_MESSAGE_BYTES);
    }

    /// **A message the *formatter* cut is still reported as truncated, on all three channels.**
    ///
    /// The half of finding W1 that is not about aborting. `format::render_bounded` stops at
    /// `liblog`'s 1,024-byte buffer, so the message reaching [`capped_record`] is already at the
    /// cap and the length comparison there sees nothing to report — a record that lost 39 KiB
    /// and looks complete, which is exactly the wrong answer its reader cannot detect. The
    /// formatter's own count is what closes that, and `capped_record_of` is where it lands.
    ///
    /// n = one record at the shape `render_bounded` produces: 1,023 bytes kept out of 40,000.
    #[test]
    fn a_message_the_formatter_cut_is_still_reported_as_truncated() {
        let kept_bytes = vec![b'x'; MAX_MESSAGE_BYTES];
        let (record, kept) = capped_record_of(Priority::Info, b"tag", &kept_bytes, 40_000);
        assert_eq!(record.message.len(), MAX_MESSAGE_BYTES, "the bytes that survived");
        assert_eq!(kept.message, MAX_MESSAGE_BYTES);
        assert_eq!(
            record.truncated,
            Some(Truncation { tag_bytes: 3, message_bytes: 40_000 }),
            "the record must carry what it was, not what arrived"
        );

        // Without the formatter's count this is what the same bytes look like: whole.
        assert_eq!(
            capped_record(Priority::Info, b"tag", &kept_bytes).0.truncated,
            None,
            "which is the silent truncation this exists to stop"
        );

        // Channel two: the ring's counter, which must move for a shortening and not for an
        // eviction.
        let ring = LogRing::new();
        ring.push(record.clone());
        assert_eq!(ring.truncated(), 1);
        assert_eq!(ring.dropped(), 0, "nothing was evicted");

        // Channel three: the rendered stderr line.
        let line = format_line(&Record {
            priority: record.priority,
            tag: &record.tag,
            message: &record.message,
            truncated: record.truncated,
        });
        assert!(line.contains(TRUNCATION_MARKER), "{}", &line[..line.len().min(120)]);
        assert!(line.contains("of 40000 bytes"), "the original length is on the line");
    }

    /// **And a message the formatter did *not* cut reports nothing**, at the boundary.
    ///
    /// `VERIFICATION.md` entry 11: a flag that is set under the fault and also set without it is
    /// a watch, not a detector. Asserted at exactly the budget and one byte past it.
    #[test]
    fn a_message_the_formatter_did_not_cut_carries_no_truncation() {
        let short = b"hello world";
        let (whole, _) = capped_record_of(Priority::Info, b"tag", short, short.len());
        assert_eq!(whole.truncated, None, "nothing was lost anywhere");
        assert_eq!(whole.message, "hello world");

        let at_budget = vec![b'x'; MAX_MESSAGE_BYTES];
        let (exact, _) =
            capped_record_of(Priority::Info, b"tag", &at_budget, MAX_MESSAGE_BYTES);
        assert_eq!(exact.truncated, None, "exactly at the buffer is not a truncation");

        let (past, _) =
            capped_record_of(Priority::Info, b"tag", &at_budget, MAX_MESSAGE_BYTES + 1);
        assert_eq!(
            past.truncated,
            Some(Truncation { tag_bytes: 3, message_bytes: MAX_MESSAGE_BYTES + 1 }),
            "one byte past it is"
        );
    }

    /// **The payload cap and the formatter's cut are different cuts and both are reported.**
    ///
    /// A long tag takes the payload, so the message loses bytes a second time — after the
    /// formatter already cut it. The record must report the larger of the two losses, which is
    /// the formatter's, or a reader would be told the message was 1,023 bytes when it was
    /// 40,000.
    #[test]
    fn a_tag_that_takes_the_payload_and_a_formatter_cut_are_both_reported() {
        let tag = vec![b't'; MAX_TAG_AND_MESSAGE_BYTES - 40];
        let kept_bytes = vec![b'x'; MAX_MESSAGE_BYTES];
        let (record, kept) = capped_record_of(Priority::Info, &tag, &kept_bytes, 40_000);
        assert_eq!(kept.tag, MAX_TAG_AND_MESSAGE_BYTES - 40, "the tag is iov[1] and fills first");
        assert_eq!(kept.message, 40, "the message gets what is left of the payload");
        assert_eq!(record.message.len(), 40);
        assert_eq!(
            record.truncated,
            Some(Truncation { tag_bytes: MAX_TAG_AND_MESSAGE_BYTES - 40, message_bytes: 40_000 }),
            "the message's own length, not the 1,023 that reached the payload cap"
        );
    }

    /// **The rendered stderr line says a record was truncated**, so a run watched only through
    /// the stream is not silently short.
    #[test]
    fn the_stderr_line_of_a_truncated_record_says_so() {
        let cut = made(4, 1 << 20);
        let line = format_line(&Record {
            priority: cut.priority,
            tag: &cut.tag,
            message: &cut.message,
            truncated: cut.truncated,
        });
        assert!(line.contains(TRUNCATION_MARKER), "{}", &line[..line.len().min(120)]);
        assert!(line.contains("of 1048576 bytes"), "the original length is on the line");

        let whole = made(4, 8);
        let line = format_line(&Record {
            priority: whole.priority,
            tag: &whole.tag,
            message: &whole.message,
            truncated: whole.truncated,
        });
        assert_eq!(line, "I/tttt: xxxxxxxx", "an uncut line is unchanged");
    }
}
