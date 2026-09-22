//! `clock_gettime`, `gettimeofday`, `gmtime_r`, `nanosleep`, `usleep`, `time`, `clock`.
//!
//! # Where the answers come from
//!
//! `omni-platform`'s [`clock`](omni_platform::clock) seam: one process-wide monotonic epoch and
//! the host wall clock. `gmtime_r` reaches neither — it is calendar arithmetic in
//! [`omni_bionic::time`], because `gmtime` is UTC by definition and needs no clock, no timezone
//! database and no locale.
//!
//! # The clock ids that are answered, and the ones that are refused
//!
//! Five are answered and the rest are refused **by number**, which is the whole of Global
//! Constraint 1 applied to an integer argument: `clock_gettime(CLOCK_THREAD_CPUTIME_ID, &ts)`
//! answered with wall time is a *number of seconds*, it is monotonic, it is plausible, and it is
//! not what was asked for. A profiler built on it would report wall time as CPU time for the life
//! of the program.
//!
//! | id | what | here |
//! |---|---|---|
//! | 0 `CLOCK_REALTIME` | wall clock | answered |
//! | 1 `CLOCK_MONOTONIC` | never jumps backwards | answered |
//! | 4 `CLOCK_MONOTONIC_RAW` | monotonic, not NTP-slewed | answered, same source |
//! | 5 `CLOCK_REALTIME_COARSE` | wall clock, cheaper and coarser | answered, same source |
//! | 6 `CLOCK_MONOTONIC_COARSE` | monotonic, cheaper and coarser | answered, same source |
//! | 2 `CLOCK_PROCESS_CPUTIME_ID` | CPU time of the process | answered, **as of phase 3e** |
//! | 3 `CLOCK_THREAD_CPUTIME_ID` | CPU time of the thread | **refused** |
//! | 7 `CLOCK_BOOTTIME` | monotonic **including** suspend | **refused** |
//!
//! **`CLOCK_PROCESS_CPUTIME_ID` was refused and is now answered, and that is a correction to
//! D22 rather than a change of mind.** Phase 3a refused it because this layer had no process CPU
//! accounting; phase 3e added [`omni_platform::process::cpu_time`] for the guest's `clock()`, and
//! leaving the refusal in place would have meant answering one question two ways — `clock()`
//! reporting a real figure while `clock_gettime` said the figure could not be had. The refusal's
//! stated reason had become false, which is the same shape as `fprintf`'s refusal text claiming
//! `omni-platform` had no file surface after phase 3b gave it one (D23).
//!
//! `CLOCK_THREAD_CPUTIME_ID` stays refused, and the distinction is real rather than tidy: a
//! per-thread figure needs `GetThreadTimes`, which is a different primitive that does not exist,
//! and answering it with the *process* figure would report every thread as having consumed the
//! whole program's CPU.
//!
//! The three "same source" rows are answers rather than approximations, and the difference
//! matters. `_COARSE` differs from its base clock only in *resolution*, and a clock that is more
//! precise than asked for satisfies the contract. `_RAW` differs only in not being slewed by NTP,
//! and nothing in this runtime slews anything. `CLOCK_BOOTTIME` is the one that genuinely differs
//! — it counts time spent suspended and the host's monotonic clock does not — so it is refused
//! rather than aliased.
//!
//! # Sleeping is capped, and that is a hostile-input defence rather than a semantics change
//!
//! `nanosleep` and `usleep` block a host thread from inside a dispatch, and the duration is a
//! number the guest chose. `nanosleep({INT64_MAX, 0})` is a permanent hang of that thread, with no
//! watchdog above it — D16's runaway-guest defence is built from *step budgets* and a sleeping
//! thread is not executing steps. So a request longer than
//! [`MAX_SLEEP_SECONDS`](super::MAX_SLEEP_SECONDS) is refused by name, with the requested
//! duration and the cap both in the message.
//!
//! A cap rather than a clamp, deliberately: clamping would return 0 after sleeping for a minute,
//! and the guest would believe it had slept for a year.
//!
//! # `-1` with `errno` versus a refusal
//!
//! The same split `guestmem` draws. A **malformed** request — `tv_nsec` outside `0..1e9`, a
//! negative `tv_sec`, a year that will not fit `int tm_year` — is what the C library itself reports
//! as `-1`/`NULL` with `errno`, so that is what happens here: it is the contract, not a stub, and
//! guest code has a defined branch for it. A request this layer **cannot carry out** is a refusal.
//!
//! A bad *pointer* is neither: it goes through [`GuestMem`](crate::mem::GuestMem) and arrives as
//! [`AbiError::BadPointer`](crate::AbiError), naming the symbol, the argument and which of
//! `admit`'s rules refused it. `-1`/`EFAULT` would be the C answer and it is the wrong one here,
//! because a guest that ignores `clock_gettime`'s return — which almost all code does — would
//! carry an unwritten `struct timespec` forward with no indication anything had happened.

use std::time::Duration;

use omni_bionic::context::GuestContext;
use omni_bionic::errno::consts;
use omni_bionic::time::{self, GmtimeError};
use omni_mem::GuestAddr;

use crate::boundary::ImportCall;
use crate::error::AbiResult;
use crate::mem::Blame;

use super::view::GuestView;
use super::{active, enter, MAX_SLEEP_SECONDS};

// ------------------------------------------------------------------ the guest's constants
//
// Linux's `clockid_t` numbering, which is what `libroblox.so` was compiled against. These are
// kernel UAPI (`include/uapi/linux/time.h`), the same source `omni-bionic`'s errno numbers come
// from, and they are stable across every Linux architecture.

/// `CLOCK_REALTIME`.
const CLOCK_REALTIME: i32 = 0;
/// `CLOCK_MONOTONIC`.
const CLOCK_MONOTONIC: i32 = 1;
/// `CLOCK_PROCESS_CPUTIME_ID`.
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
/// `CLOCK_THREAD_CPUTIME_ID`.
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
/// `CLOCK_MONOTONIC_RAW`.
const CLOCK_MONOTONIC_RAW: i32 = 4;
/// `CLOCK_REALTIME_COARSE`.
const CLOCK_REALTIME_COARSE: i32 = 5;
/// `CLOCK_MONOTONIC_COARSE`.
const CLOCK_MONOTONIC_COARSE: i32 = 6;
/// `CLOCK_BOOTTIME`.
const CLOCK_BOOTTIME: i32 = 7;

/// Nanoseconds in a second, as the bound `tv_nsec` must respect.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// `CLOCKS_PER_SEC`, the unit `clock()` reports in.
///
/// **A million, fixed by POSIX for every conforming system**, and bionic defines it so. It is not
/// the resolution of anything: the host's process-time accounting is far coarser (see
/// [`omni_platform::process::cpu_time`]) and that changes the granularity of the answer without
/// changing its unit.
const CLOCKS_PER_SEC: i64 = 1_000_000;

/// Bytes of a guest `struct timespec` and `struct timeval`: two `long`-sized fields on LP64.
const PAIR_BYTES: usize = 16;

/// `EOVERFLOW`, the code `gmtime_r` reports when the year will not fit `int tm_year`.
///
/// Linux UAPI value (75). **Aliased to `omni-bionic`'s table rather than repeated.** It was spelled
/// out here on the argument that the table "carries only the codes that crate's own functions
/// produce" — phase 3b's file-io group put `EOVERFLOW` in that table and invalidated it, leaving two
/// sources of truth for one guest-ABI number. They agreed; that is luck, not a design.
const EOVERFLOW: i32 = omni_bionic::errno::consts::EOVERFLOW;

/// The symbolic name of a `clockid_t` this layer knows about, for a refusal that has to say what
/// was asked for.
fn clock_name(id: i32) -> Option<&'static str> {
    Some(match id {
        CLOCK_REALTIME => "CLOCK_REALTIME",
        CLOCK_MONOTONIC => "CLOCK_MONOTONIC",
        CLOCK_PROCESS_CPUTIME_ID => "CLOCK_PROCESS_CPUTIME_ID",
        CLOCK_THREAD_CPUTIME_ID => "CLOCK_THREAD_CPUTIME_ID",
        CLOCK_MONOTONIC_RAW => "CLOCK_MONOTONIC_RAW",
        CLOCK_REALTIME_COARSE => "CLOCK_REALTIME_COARSE",
        CLOCK_MONOTONIC_COARSE => "CLOCK_MONOTONIC_COARSE",
        CLOCK_BOOTTIME => "CLOCK_BOOTTIME",
        _ => return None,
    })
}

/// Split a duration into the `(seconds, nanoseconds)` a guest `struct timespec` holds.
///
/// # Errors
///
/// [`AbiError::Refused`](crate::AbiError::Refused) if the seconds do not fit a signed 64-bit
/// `time_t`. Unreachable from the
/// clocks here — it is 292 billion years — and checked rather than cast, because `as` on a
/// `Duration` that somehow held more would produce a *negative* time.
fn split(view: &GuestView<'_>, duration: Duration) -> AbiResult<(i64, i64)> {
    let seconds = i64::try_from(duration.as_secs()).map_err(|_| {
        view.refusal(format!(
            "the clock reads {} seconds, which does not fit the guest's signed 64-bit time_t",
            duration.as_secs()
        ))
    })?;
    Ok((seconds, i64::from(duration.subsec_nanos())))
}

/// Write the two `long`-sized fields of a `struct timespec` or `struct timeval`.
///
/// One 16-byte write rather than two 8-byte ones, so a destination that is only half writable
/// leaves the guest nothing rather than half a timestamp.
fn write_pair(
    view: &GuestView<'_>,
    at: u64,
    first: i64,
    second: i64,
    argument: usize,
) -> AbiResult<()> {
    let address = guest_address(view, at)?;
    let mut bytes = [0u8; PAIR_BYTES];
    bytes[..8].copy_from_slice(&first.to_le_bytes());
    bytes[8..].copy_from_slice(&second.to_le_bytes());
    view.mem().write_bytes(address, &bytes, Blame::new(view.symbol(), view.address(), argument))
}

/// Narrow a guest pointer to a host address, refusing rather than truncating.
fn guest_address(view: &GuestView<'_>, pointer: u64) -> AbiResult<GuestAddr> {
    GuestAddr::try_from(pointer)
        .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))
}

/// `int clock_gettime(clockid_t clk_id, struct timespec *tp)`
pub(super) fn clock_gettime(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (clk_id, tp) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    {
        let view = enter(c, &state);
        let duration = match clk_id {
            CLOCK_REALTIME | CLOCK_REALTIME_COARSE => omni_platform::clock::realtime_now(),
            CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE => {
                omni_platform::clock::monotonic_now()
            }
            CLOCK_PROCESS_CPUTIME_ID => process_cpu_time(&view)?,
            other => {
                let named = clock_name(other)
                    .map_or_else(|| "no clock this layer has a name for".to_string(), |name| format!("`{name}`"));
                return Err(view.refusal(format!(
                    "the guest asked for clockid_t {other} ({named}). This layer models the \
                     realtime and monotonic clocks and the PROCESS cpu clock, and nothing else: \
                     it has no per-thread CPU accounting -- that is `GetThreadTimes`, a \
                     primitive `omni-platform` does not have, and answering it with the process \
                     figure would report every thread as having burned the whole program's CPU \
                     -- and no way to know how long the host was suspended, which is what \
                     CLOCK_BOOTTIME counts and the monotonic clock does not. Answering with wall \
                     time would be a plausible number of seconds and would not be what was asked \
                     for"
                )));
            }
        };
        let (seconds, nanos) = split(&view, duration)?;
        write_pair(&view, tp, seconds, nanos, 1)?;
    }
    c.ret().i32(0);
    Ok(())
}

/// `int gettimeofday(struct timeval *tv, struct timezone *tz)`
///
/// `tv` may legitimately be null — the call is then only about `tz` — so a null `tv` writes
/// nothing and succeeds rather than faulting.
///
/// `tz` is the obsolete `struct timezone { int tz_minuteswest; int tz_dsttime; }`. Linux fills it
/// with **zeroes** and has done since the field stopped meaning anything; writing zeroes is
/// therefore the kernel's own answer rather than a placeholder, and it is what a guest that passes
/// a non-null `tz` will read on a real device.
pub(super) fn gettimeofday(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (tv, tz) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    {
        let view = enter(c, &state);
        if tv != 0 {
            let (seconds, nanos) = split(&view, omni_platform::clock::realtime_now())?;
            // `tv_usec`, not `tv_nsec`: a `struct timeval` is microseconds. Writing nanoseconds
            // here is a thousand-fold error that still looks like a time.
            write_pair(&view, tv, seconds, nanos / 1_000, 0)?;
        }
        if tz != 0 {
            let at = guest_address(&view, tz)?;
            view.mem().write_bytes(at, &[0u8; 8], Blame::new(view.symbol(), view.address(), 1))?;
        }
    }
    c.ret().i32(0);
    Ok(())
}

/// `struct tm *gmtime_r(const time_t *timer, struct tm *result)`
///
/// Returns `result` on success and `NULL` with `EOVERFLOW` when the year does not fit
/// `int tm_year` — which is what glibc does and is a contract rather than a stub. See
/// [`omni_bionic::time`] for the calendar arithmetic and for why it is branch-free.
///
/// `tm_zone` points at a NUL-terminated `"UTC"` in the adapter's own pool, interned once when the
/// instance was built. It has to point at *something*: `tm_zone` is a `const char *` and guest
/// code prints it.
pub(super) fn gmtime_r(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (timer, result) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let returned = {
        let mut view = enter(c, &state);
        let timer_at = guest_address(view.blaming(0), timer)?;
        // `read_u64` then reinterpret: `time_t` is signed and the bits are the same, but the
        // reinterpretation is spelled so that a pre-1970 timestamp is obviously intended.
        let timestamp =
            view.mem().read_u64(timer_at, Blame::new(view.symbol(), view.address(), 0))? as i64;
        match time::gmtime(timestamp) {
            Err(GmtimeError::YearOutOfRange { .. }) => {
                view.set_errno(EOVERFLOW);
                0u64
            }
            Ok(tm) => {
                let zone = state.bionic.utc_zone();
                let at = guest_address(view.blaming(1), result)?;
                // The bionic crate reports a failed write as a thin `Fault`; the view turns it
                // back into the boundary's rich error, naming the argument and the rule.
                if let Err(fault) = time::write_tm(&mut view, at as u64, &tm, zone as u64) {
                    return Err(view.fault(fault));
                }
                result
            }
        }
    };
    c.ret().u64(returned);
    Ok(())
}

/// `time_t mktime(struct tm *tm)`
///
/// **MEASURED, and it is the TLS certificate check.** With the raw `getrandom` answered, M6's
/// network run got the client-settings request onto the wire and the guest thread carrying it
/// died here: `GuestThreadFailure { thread: 6, why: "the guest called the imported symbol
/// `mktime` through its thunk at 0x1e172e54ef0, and nothing in the compatibility layer implements
/// it" }`, from image offset 0x2212920. The engine carries its own OpenSSL (D30), which parses an
/// X.509 `notBefore`/`notAfter` into a `struct tm` and calls this to compare it with now.
///
/// # On this runtime `mktime` **is** `timegm`, and that is a fact rather than a simplification
///
/// C says `mktime` interprets the broken-down time as **local**. This process has no local time:
/// there is no timezone database, no `TZ` in the environment (`getenv` answers `NULL` for every
/// name, which is a fact about a process started with no environment, not a stub), and no
/// `localtime`/`localtime_r` is bound or imported on any reached path. Everything this layer
/// reports is UTC -- `gmtime_r` writes `tm_gmtoff = 0` and a `tm_zone` of `"UTC"`, which is the
/// storage this handler reuses.
///
/// So the honest statement is: **this runtime's local time is UTC**, and `mktime` is therefore
/// `timegm`. That is a real difference from a device, which would apply the phone's offset, and
/// it is written here rather than hidden because it is observable: a certificate whose validity
/// window is being compared against `time()` -- which is also UTC here -- sees a consistent
/// clock, while guest code that formatted a local timestamp for a user would see UTC.
/// **What would falsify the "nothing is affected" half**: a run in which the guest reads `TZ` or
/// calls `localtime`. Both would arrive by name -- `getenv("TZ")` through the env table and
/// `localtime` as an `Unbound` -- so neither can happen quietly.
///
/// # What is written back, and the order it is written in
///
/// C 7.29.2.3 requires `mktime` to **normalise the structure in place**: the 61st second becomes
/// the next minute, month 12 becomes January of the next year, and `tm_wday`/`tm_yday` are set
/// from the date. `omni_bionic::time::mktime` does that arithmetic and this writes the result
/// back through the same one-piece [`omni_bionic::time::write_tm`] `gmtime_r` uses, so a
/// destination that is not fully writable faults with nothing written rather than leaving the
/// guest a half-normalised time.
///
/// **The structure is written before the value is returned, and only on success.** An
/// unrepresentable date is `(time_t)-1` with the guest's `struct tm` **untouched**, which is what
/// C requires -- "the values of the other components are set to represent the specified calendar
/// time" is conditional on success, and a caller that got `-1` and a rewritten `tm` could not
/// tell which fields were its own.
pub(super) fn mktime(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let at = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let returned = {
        let mut view = enter(c, &state);
        let tm_at = guest_address(view.blaming(0), at)?;
        let guest_tm = match time::read_tm(&view, tm_at as u64) {
            Ok(read) => read,
            Err(fault) => return Err(view.fault(fault)),
        };
        match time::mktime(&guest_tm.tm) {
            // C: `(time_t)-1`, and the guest's structure is left exactly as it was.
            Err(_) => -1i64,
            Ok((seconds, normalised)) => {
                let zone = state.bionic.utc_zone();
                if let Err(fault) = time::write_tm(&mut view, tm_at as u64, &normalised, zone as u64)
                {
                    return Err(view.fault(fault));
                }
                seconds
            }
        }
    };
    c.ret().u64(returned as u64);
    Ok(())
}

/// `size_t strftime(char *s, size_t max, const char *format, const struct tm *tm)`
///
/// **The symbol M4's gate stopped on.** `nativeInitFastLog` is one of the two scripted downcalls
/// of §8 step 9 that did not return, and the reason was recorded plainly: there was no
/// implementation to bind. There is now — `omni_bionic::time::strftime` — and this is the
/// binding.
///
/// # What it returns, and the one answer that is not a length
///
/// C17 7.27.3.5: the number of bytes written **not counting the terminating NUL**, or **0** if
/// the result including the NUL would not fit in `max` — and when it returns 0 the contents of
/// the array are **indeterminate**. So the zero case writes *nothing* into guest memory rather
/// than a truncated string: a caller that received a truncated timestamp and a zero could not
/// tell it from a caller that received nothing, and the truncation is the shape that produces
/// wrong *text* downstream.
///
/// # Every refusal names the conversion, and none of them guesses
///
/// `omni-bionic` refuses a conversion it cannot perform correctly rather than emitting something —
/// `%s` needs `mktime` and the tz database, `%k`/`%l`/`%P`/`%v`/`%+` are extensions outside C and
/// POSIX, and an unknown conversion is refused rather than copied through as literal text. That
/// last one is the important one: tzcode-derived libraries emit the literal `%Q` for an unknown
/// `%Q`, and nothing downstream can tell that from a time.
///
/// A field outside its POSIX range is refused **per conversion**, which is why a `tm` the guest
/// scribbled on fails where it is read rather than producing a date.
pub(super) fn strftime(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (s, max, format, tm) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    format_time(c, s, max, format, tm)
}

/// `size_t strftime_l(char *s, size_t max, const char *format, const struct tm *tm, locale_t)`
///
/// **The locale argument selects nothing, on this runtime or on a device**, and that is a fact
/// about bionic rather than a simplification. Android has one locale implementation: every name
/// `newlocale` accepts maps to it, which is why `omni_bionic::locale::newlocale` answers the same
/// handle for all of them and why `__ctype_get_mb_cur_max` is 4 with no locale that changes it.
/// bionic's own `strftime_l` is a one-line forward to `strftime` for exactly this reason, and so
/// is this.
///
/// It is **read** rather than ignored, so that a caller passing a handle this layer never issued
/// is a refusal naming the value instead of a timestamp formatted by luck. That is the whole of
/// the difference between forwarding and dropping an argument.
///
/// Reached in M6: `nativePostClientSettingsLoadedInitialization3` calls it at guest `0x06258e44`,
/// which is one call past the point where the client settings have been parsed and the engine
/// starts reporting its own build. `jni-surface.md` records `strftime_l` in the file's LAST
/// section as "there too and not reached" — it is reached now, one §8 row later than the note.
pub(super) fn strftime_l(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (s, max, format, tm, locale) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    {
        let view = enter(c, &state);
        if locale != 0 && locale != omni_bionic::locale::C_LOCALE_HANDLE {
            return Err(view.refusal(format!(
                "`strftime_l` was given locale {locale:#x}, which is not a handle this layer                  issued. `newlocale` answers one handle here and bionic has one locale                  implementation, so a different value is a stale or fabricated `locale_t` rather                  than a locale whose formatting differs"
            )));
        }
    }
    format_time(c, s, max, format, tm)
}

/// The body `strftime` and `strftime_l` share, which is all of both of them.
///
/// One copy, for the reason the boundary keeps one marshaller: a second implementation is how the
/// two would come to disagree about the zero return, the NUL, or which conversions refuse. The
/// messages name `c.symbol()`, so each reports itself.
fn format_time(
    c: &mut ImportCall<'_, '_>,
    s: u64,
    max: u64,
    format: u64,
    tm: u64,
) -> AbiResult<()> {
    let state = active(c.symbol(), c.address())?;
    let written = {
        let view = enter(c, &state);
        if format == 0 || tm == 0 {
            return Err(view.refusal(format!(
                "`strftime` was given a null {}",
                if format == 0 { "format string" } else { "struct tm" }
            )));
        }
        let format_at = guest_address(view.blaming(2), format)?;
        let format_bytes =
            view.mem().cstr(format_at, Blame::new(view.symbol(), view.address(), 2))?;
        let tm_at = guest_address(view.blaming(3), tm)?;
        let guest_tm = match time::read_tm(&view, tm_at as u64) {
            Ok(read) => read,
            Err(fault) => return Err(view.fault(fault)),
        };
        // `tm_zone` is a `const char *` **into guest memory**, and only `%Z` reads it. Read it
        // here, where the view is alive, rather than inside the formatter: `omni-bionic` has no
        // guest memory and must not grow any (D19).
        let zone_bytes = if guest_tm.zone == 0 {
            None
        } else {
            let zone_at = guest_address(view.blaming(3), guest_tm.zone)?;
            Some(view.mem().cstr(zone_at, Blame::new(view.symbol(), view.address(), 3))?)
        };
        let when = time::StrftimeTm {
            tm: guest_tm.tm,
            gmtoff: guest_tm.gmtoff,
            zone: zone_bytes.as_deref(),
        };
        // `max` is a `size_t` the guest chose. Narrowed rather than truncated: a `max` wider than
        // a host `usize` cannot describe a buffer this process could address, and wrapping it
        // would turn a huge buffer into a small one and a correct result into a silent zero.
        let Ok(max) = usize::try_from(max) else {
            return Err(view.refusal(format!(
                "`strftime` was given max = {max}, which is wider than this host's usize and so \
                 cannot describe a buffer in this address space"
            )));
        };
        match time::strftime(max, &format_bytes, &when) {
            Err(why) => return Err(view.refusal(why.to_string())),
            // C: the array's contents are indeterminate, so nothing is written. A truncated
            // string beside a zero is the believable wrong answer, and it is one a caller that
            // checked the return would still carry forward.
            Ok(time::StrftimeOutput::DoesNotFit { .. }) => 0u64,
            Ok(time::StrftimeOutput::Fits(bytes)) => {
                if s == 0 {
                    return Err(view.refusal(
                        "`strftime` produced a result and was given a null destination"
                            .to_string(),
                    ));
                }
                let at = guest_address(view.blaming(0), s)?;
                // The bytes **and** the NUL, in one access: a destination that is only partly
                // writable leaves the guest nothing rather than an unterminated string, which is
                // the direction review finding M1 says to err in.
                let mut with_nul = bytes;
                let length = with_nul.len();
                with_nul.push(0);
                view.mem().write_bytes(
                    at,
                    &with_nul,
                    Blame::new(view.symbol(), view.address(), 0),
                )?;
                length as u64
            }
        }
    };
    c.ret().u64(written);
    Ok(())
}

/// `struct tm *gmtime(const time_t *timer)`
///
/// The same calendar arithmetic as [`gmtime_r`], into **this thread's** `struct tm` rather than
/// one the caller supplies -- which is what the non-`_r` form is: a pointer to storage the
/// library owns and the caller must not free. Per thread rather than per process, which is what
/// bionic does and what stops two guest threads overwriting each other's result.
///
/// **Not among the 188 statically-reachable imports.** M4's gate found it: the engine calls it
/// from `NativeSettingsInterface.nativeInitFastLog`, which formats a timestamp for every log
/// line. D17 records 188 as a lower bound.
///
/// A year outside `int tm_year` is `EOVERFLOW` and a null return, exactly as [`gmtime_r`]
/// answers it -- the one case where the two must agree and the easiest place for them to drift.
pub(super) fn gmtime(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let timer = {
        let mut a = c.args();
        a.next_u64()?
    };
    let state = active(c.symbol(), c.address())?;
    let returned = {
        let mut view = enter(c, &state);
        let timer_at = guest_address(view.blaming(0), timer)?;
        let timestamp =
            view.mem().read_u64(timer_at, Blame::new(view.symbol(), view.address(), 0))? as i64;
        match time::gmtime(timestamp) {
            Err(GmtimeError::YearOutOfRange { .. }) => {
                view.set_errno(EOVERFLOW);
                0u64
            }
            Ok(tm) => {
                let zone = state.bionic.utc_zone();
                let at = view.tm_address();
                if let Err(fault) = time::write_tm(&mut view, at as u64, &tm, zone as u64) {
                    return Err(view.fault(fault));
                }
                at as u64
            }
        }
    };
    c.ret().u64(returned);
    Ok(())
}

/// The duration a `(seconds, nanoseconds)` pair asks for, or the errno a malformed one gets.
///
/// POSIX: `tv_nsec` must be in `[0, 999999999]` and `tv_sec` must not be negative; anything else
/// is `EINVAL`. That is the C library's own answer to a malformed request, which is why it is
/// returned rather than refused.
fn requested(seconds: i64, nanos: i64) -> Result<Duration, i32> {
    if seconds < 0 || !(0..NANOS_PER_SECOND).contains(&nanos) {
        return Err(consts::EINVAL);
    }
    // Both casts are safe: the checks above establish `seconds >= 0` and `nanos` in range.
    Ok(Duration::new(seconds as u64, nanos as u32))
}

/// Whether a requested sleep is past [`MAX_SLEEP_SECONDS`] and must be refused.
///
/// A predicate of its own rather than an inline comparison, so that it can be asserted **without
/// sleeping**. A test for "an over-long sleep is refused" that got the answer wrong would hang for
/// as long as the guest asked, which is not a failure mode a suite can recover from — so the
/// decision is checked here as arithmetic and end to end in `tests/bionic.rs` with a value the cap
/// really does refuse.
fn capped(duration: Duration) -> bool {
    duration.as_secs() > MAX_SLEEP_SECONDS
}

/// Sleep, or refuse a duration past the cap.
///
/// Returns the errno to report, or an error if the request is refused. See the module
/// documentation: the cap is a hostile-input defence, and a clamp would be a lie.
fn sleep_for(view: &GuestView<'_>, duration: Duration, asked: &str) -> AbiResult<()> {
    if capped(duration) {
        return Err(view.refusal(format!(
            "the guest asked to sleep for {asked}, and this layer caps a single sleep at \
             {MAX_SLEEP_SECONDS} seconds. A sleeping thread executes no guest instructions, so \
             D16's step-budget watchdog cannot end it and the host thread would be blocked for as \
             long as the guest said. Clamping the sleep instead would return success from a call \
             that had not done what it was asked"
        )));
    }
    omni_platform::clock::sleep(duration);
    Ok(())
}

/// `int nanosleep(const struct timespec *req, struct timespec *rem)`
///
/// `rem` is the remaining time when a sleep is cut short by a signal. Nothing here delivers
/// signals to the guest, so every sleep that starts runs to completion and `rem` is written as
/// zero — a fact about this runtime, not a placeholder. A `rem` the caller did not supply is
/// skipped rather than faulted on: null is how a caller says it does not want it.
pub(super) fn nanosleep(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (req, rem) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let code = {
        let mut view = enter(c, &state);
        let req_at = guest_address(view.blaming(0), req)?;
        let blame = Blame::new(view.symbol(), view.address(), 0);
        let seconds = view.mem().read_u64(req_at, blame)? as i64;
        let nanos = view.mem().read_u64(req_at + 8, blame)? as i64;
        match requested(seconds, nanos) {
            Err(errno) => {
                view.set_errno(errno);
                -1
            }
            Ok(duration) => {
                sleep_for(&view, duration, &format!("{seconds} s + {nanos} ns"))?;
                if rem != 0 {
                    write_pair(&view, rem, 0, 0, 1)?;
                }
                0
            }
        }
    };
    c.ret().i32(code);
    Ok(())
}

/// `int usleep(useconds_t usec)`
///
/// `useconds_t` is `unsigned int` — **32 bits**, not 64 — so only the low half of `X0` is the
/// argument and the high half is whatever the caller left there. Reading all 64 bits would turn a
/// dirty register into a multi-century sleep request, which the cap would then refuse: a correct
/// call refused because of a register nobody was required to clear.
///
/// bionic's `usleep` has no `EINVAL` for a value at or above one million — it converts and calls
/// `nanosleep` — so neither does this.
pub(super) fn usleep(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let micros = u64::from(c.args().next_u64()? as u32);
    let state = active(c.symbol(), c.address())?;
    {
        let view = enter(c, &state);
        sleep_for(&view, Duration::from_micros(micros), &format!("{micros} us"))?;
    }
    c.ret().i32(0);
    Ok(())
}

// ================================================================== phase 3e: the two left behind

/// This process's consumed CPU time, or a refusal naming the platform's own reason.
///
/// The refusal is what a Linux or macOS build produces today, and it names
/// `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` because [`omni_platform::process::cpu_time`]'s
/// structural backend does. Nothing here invents a figure when the seam has none: `clock()`
/// answering `0` would say the process has used no CPU, which is a number a profiler divides by.
fn process_cpu_time(view: &GuestView<'_>) -> AbiResult<Duration> {
    omni_platform::process::cpu_time().map_err(|error| {
        view.refusal(format!(
            "this layer cannot read the process's consumed CPU time: {error}"
        ))
    })
}

/// `time_t time(time_t *tloc)`
///
/// **One line over the wall clock, and it is deliberately the same source `gettimeofday` uses.**
/// A guest that called both and compared them would otherwise be able to see two clocks where a
/// device has one.
///
/// `tloc` may be null, which is the ordinary form (`time(NULL)`); a non-null one receives the same
/// value that is returned. The write happens **before** the value is returned, so a `tloc` that is
/// not writable guest memory fails the call rather than returning a time the guest then believes
/// it also stored.
pub(super) fn time(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let tloc = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let seconds = {
        let view = enter(c, &state);
        let (seconds, _nanos) = split(&view, omni_platform::clock::realtime_now())?;
        if tloc != 0 {
            let at = guest_address(&view, tloc)?;
            view.mem().write_u64(
                at,
                seconds as u64,
                Blame::new(view.symbol(), view.address(), 0),
            )?;
        }
        seconds
    };
    // `time_t` is a signed 64-bit value on LP64, so the whole register is the answer.
    c.ret().u64(seconds as u64);
    Ok(())
}

/// `clock_t clock(void)`
///
/// Processor time this **process** has consumed, in units of `CLOCKS_PER_SEC`.
///
/// # Three ways to get this wrong, and what each would look like
///
/// **`CLOCKS_PER_SEC` is 1,000,000 and is not the host's.** It is fixed at a million by POSIX for
/// every conforming system and bionic defines it so; the granularity of the underlying clock has
/// nothing to do with it. A `clock()` scaled to anything else divides by the wrong number
/// everywhere `clock()` is used, which is always a ratio of two readings, so the error is a
/// constant factor that never looks like a units bug.
///
/// **It is CPU time, not wall time.** `omni_platform::process::cpu_time` is `GetProcessTimes`
/// here; using `monotonic_now` would produce a monotonic, plausible, wrong number that a guest
/// benchmark would report as CPU seconds.
///
/// **`clock_t` is signed and 64-bit on LP64**, so the value is returned as the whole of `X0`.
/// Truncating to 32 bits would wrap after about 36 minutes of CPU time, and the wrap would look
/// like the process suddenly running backwards.
///
/// A host that cannot report the figure is a **refusal**, not `(clock_t)-1`. `-1` is C's own
/// error return and would be the defensible answer if this layer had *asked* and been refused by
/// the OS — but on Linux and macOS the seam has no implementation at all, and a guest that reads
/// `-1` learns that the call failed rather than that Omnidroid has not built this yet.
pub(super) fn clock(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let state = active(c.symbol(), c.address())?;
    let ticks = {
        let view = enter(c, &state);
        let cpu = process_cpu_time(&view)?;
        // `CLOCKS_PER_SEC` is a million, so this is whole microseconds. `as_micros` is a `u128`
        // and the conversion is checked rather than cast: 2^63 microseconds is 292,000 years of
        // CPU time, so the failure is unreachable, and a cast that wrapped would hand the guest a
        // negative `clock_t`.
        i64::try_from(cpu.as_micros()).map_err(|_| {
            view.refusal(format!(
                "the process has consumed {cpu:?} of CPU time, which does not fit the guest's \
                 signed 64-bit clock_t at CLOCKS_PER_SEC = {CLOCKS_PER_SEC}"
            ))
        })?
    };
    c.ret().u64(ticks as u64);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_bionic::time::TM_BYTES;

    /// The clock-id table is the Linux UAPI numbering, and the refusal set is the complement.
    #[test]
    fn the_clock_ids_are_the_linux_numbering() {
        assert_eq!(
            [
                CLOCK_REALTIME,
                CLOCK_MONOTONIC,
                CLOCK_PROCESS_CPUTIME_ID,
                CLOCK_THREAD_CPUTIME_ID,
                CLOCK_MONOTONIC_RAW,
                CLOCK_REALTIME_COARSE,
                CLOCK_MONOTONIC_COARSE,
                CLOCK_BOOTTIME,
            ],
            [0, 1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(clock_name(7), Some("CLOCK_BOOTTIME"));
        assert_eq!(clock_name(11), None, "an unknown id must not be given a name it does not have");
    }

    /// POSIX's `timespec` validity rule, at both edges.
    #[test]
    fn a_malformed_timespec_is_einval_and_a_valid_one_is_a_duration() {
        assert_eq!(requested(-1, 0), Err(consts::EINVAL), "a negative tv_sec");
        assert_eq!(requested(0, -1), Err(consts::EINVAL), "a negative tv_nsec");
        assert_eq!(
            requested(0, NANOS_PER_SECOND),
            Err(consts::EINVAL),
            "tv_nsec must be strictly below one second"
        );
        assert_eq!(requested(0, NANOS_PER_SECOND - 1), Ok(Duration::new(0, 999_999_999)));
        assert_eq!(requested(0, 0), Ok(Duration::ZERO));
        assert_eq!(requested(2, 500), Ok(Duration::new(2, 500)));
        // i64::MAX seconds is well-formed and is what the *cap* exists to refuse, not this check.
        assert_eq!(requested(i64::MAX, 0), Ok(Duration::new(i64::MAX as u64, 0)));
    }

    /// The sleep cap, asserted as arithmetic rather than by sleeping.
    ///
    /// **This is the detector for the cap**, and it is a unit test on purpose: the end-to-end form
    /// of "an over-long sleep is refused" cannot fail safely, because a version that did not
    /// refuse would sleep for the `i64::MAX` seconds the test asked for and hang the suite rather
    /// than failing it. Here the same decision is a pure function over a `Duration`.
    #[test]
    fn the_sleep_cap_refuses_past_a_minute_and_admits_everything_under_it() {
        assert_eq!(MAX_SLEEP_SECONDS, 60);
        assert!(!capped(Duration::ZERO));
        assert!(!capped(Duration::from_millis(10)), "an ordinary sleep must not be refused");
        assert!(!capped(Duration::from_millis(1)));
        assert!(!capped(Duration::from_secs(MAX_SLEEP_SECONDS)), "the cap is inclusive");
        assert!(
            !capped(Duration::new(MAX_SLEEP_SECONDS, 999_999_999)),
            "and it is whole seconds, so the last nanosecond of the last second is still in"
        );
        assert!(capped(Duration::from_secs(MAX_SLEEP_SECONDS + 1)));
        assert!(capped(Duration::new(i64::MAX as u64, 0)), "the case this exists for");
    }

    /// A `struct timespec` and a `struct timeval` are both two 8-byte fields on LP64.
    #[test]
    fn the_pair_is_sixteen_bytes() {
        assert_eq!(PAIR_BYTES, 16);
        assert_eq!(TM_BYTES, 56, "and a struct tm is not the same shape");
    }
}
