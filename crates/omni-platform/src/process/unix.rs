//! Shared unix body of the process seam, used by the [`linux`](super::linux) and
//! [`macos`](super::macos) backends.
//!
//! # Status: one call implemented and run on Linux, four structural for macOS
//!
//! **[`cpu_time`] is implemented here and has been run on Linux** (x86-64, kernel 7.0, glibc
//! 2.43): `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` is POSIX and is the same call on macOS 10.12
//! and later, so it is written once for both. It has **not** been run on macOS.
//!
//! The other four -- entropy, the current cpu, a thread's nice value, the machine's maker -- are
//! **not POSIX-common**, and each is implemented in [`linux`](super::linux) with the call that
//! target has: `getrandom(2)`, `sched_getcpu(3)`, `setpriority(PRIO_PROCESS, gettid())` and
//! `/sys/class/dmi/id/sys_vendor`. The bodies below stay the structural refusals they were,
//! because macOS's backend re-exports them and none of the four has one spelling on both targets
//! (`arc4random_buf`; no `sched_getcpu` at all; `setpriority` per *process*; `IOPlatformExpertDevice`).
//! On Linux they are compiled and unused, which is what the `allow(dead_code)` on each says.
//!
//! This is the same deliberate reading of Global Constraint 1 that [`crate::vm::unix`] records: an
//! unverified body misbehaves silently where a typed error fails visibly. It matters more here
//! than it looks, because both of these calls have a *believable* wrong answer available —
//! `random_bytes` could fill with a pseudo-random sequence and `current_cpu` could return 0, and
//! each would pass every test that only checked the call returned.

use std::time::Duration;

use super::{ProcessError, ProcessResult};

/// The platform this backend was compiled for, for error messages.
#[cfg_attr(target_os = "linux", allow(dead_code))] // only the macOS-only refusals below use it
fn platform() -> &'static str {
    std::env::consts::OS
}

#[cfg_attr(target_os = "linux", allow(dead_code))] // only the macOS-only refusals below use it
fn unsupported<T>(operation: &'static str, intended: &'static str) -> ProcessResult<T> {
    Err(ProcessError::Unsupported { operation, intended, platform: platform() })
}

/// Intended: `getrandom(buf, len, 0)` on Linux, `arc4random_buf(buf, len)` on macOS.
///
/// The decision in it: **`getrandom` can block** before the kernel entropy pool is initialised,
/// and it returns short. A correct implementation loops on partial fills and handles `EINTR`, and
/// must decide whether `GRND_NONBLOCK` plus a failure is better than a boot-time stall — a
/// question about the host, not about this seam. `arc4random_buf` on macOS cannot fail and cannot
/// return short, so the two implementations are not the same shape and sharing a body between them
/// would be wrong.
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn random_bytes(_out: &mut [u8]) -> ProcessResult<()> {
    unsupported("random_bytes", "getrandom(2) on Linux, arc4random_buf(3) on macOS")
}

/// Intended: `sched_getcpu()` on Linux.
///
/// The decision in it: **macOS has no `sched_getcpu` and no supported equivalent.** There is no
/// public API that names the current core; the nearest thing is a private `cpu_number()` in
/// `<cpuid.h>`'s vicinity, which is not a stable interface. So the macOS arm of this is expected
/// to stay a refusal, and the guest symbol it serves — `sched_getcpu` — is expected to be refused
/// by name there rather than answered. That is a real and permanent difference between the two
/// unix targets, and it is the reason this module says "on Linux" rather than "on unix".
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn current_cpu() -> ProcessResult<u32> {
    unsupported("current_cpu", "sched_getcpu(3) on Linux; macOS has no supported equivalent")
}

/// `clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &ts)`, on both targets, in nanoseconds.
///
/// **Not `getrusage(RUSAGE_SELF)`**, and the decision below is why.
///
/// The decision in it: **`CLOCK_PROCESS_CPUTIME_ID` and `getrusage` do not report the same
/// thing to the same precision.** The clock id is nanosecond-resolution and counts the whole
/// process; `getrusage` reports `ru_utime` and `ru_stime` as `timeval`s, which is microseconds,
/// and a caller has to add them. Picking one and documenting the other as equivalent would be
/// the plausible-looking mistake — the guest's `clock()` is scaled to `CLOCKS_PER_SEC`, which is
/// 1,000,000, so microseconds happen to be exactly enough for it and are *not* enough for
/// `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)`, which this same primitive also serves.
///
/// macOS has `CLOCK_PROCESS_CPUTIME_ID` since 10.12 and it is the same call there, which is why
/// this one entry is genuinely shared between the two targets where [`current_cpu`] is not.
pub(super) fn cpu_time() -> ProcessResult<Duration> {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `clock_gettime` writes one `timespec` through the pointer and reads nothing else;
    // `now` is a live, uniquely-borrowed local of exactly that type.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &raw mut now) };
    if rc != 0 {
        return Err(ProcessError::Errno {
            operation: "cpu_time",
            api: "clock_gettime(CLOCK_PROCESS_CPUTIME_ID)",
            errno: last_errno(),
        });
    }
    // A CPU-time clock counts up from zero: a negative field is not a duration this process
    // consumed, and `as u64` would turn one into five hundred billion years. Reported, not cast.
    let (Ok(seconds), Ok(nanos)) = (u64::try_from(now.tv_sec), u32::try_from(now.tv_nsec)) else {
        return Err(ProcessError::Indeterminate {
            operation: "cpu_time",
            detail: format!(
                "clock_gettime(CLOCK_PROCESS_CPUTIME_ID) answered {{ tv_sec: {}, tv_nsec: {} }}, \
                 which is not an amount of processor time",
                now.tv_sec, now.tv_nsec
            ),
        });
    };
    Ok(Duration::new(seconds, nanos))
}

/// This thread's `errno`, read straight after the call that set it.
///
/// `std::io::Error::last_os_error` is that read, portable across both unix targets (it is
/// `__errno_location` on glibc and `__error` on macOS), so this crate does not spell either.
pub(super) fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Intended: `setpriority(PRIO_PROCESS, gettid(), nice)` on Linux, where a nice value is per
/// thread and the call is the guest's own; on macOS, whose `setpriority` is per process, a
/// thread's QoS class or `pthread_setschedparam` -- a mapping decision still to be made.
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn set_current_thread_nice(nice: i32) -> ProcessResult<()> {
    let _ = nice;
    unsupported(
        "set_current_thread_nice",
        "setpriority(PRIO_PROCESS, gettid(), nice) on Linux; a thread QoS class on macOS",
    )
}

/// Intended: `/sys/class/dmi/id/sys_vendor` on Linux; the `IOPlatformExpertDevice`'s
/// `manufacturer` property on macOS.
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn host_manufacturer() -> ProcessResult<String> {
    unsupported(
        "host_manufacturer",
        "/sys/class/dmi/id/sys_vendor on Linux; IOPlatformExpertDevice's manufacturer on macOS",
    )
}

/// Intended: `getpriority(PRIO_PROCESS, gettid())` on Linux; the thread's QoS class on macOS.
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn current_thread_host_priority() -> ProcessResult<i32> {
    unsupported(
        "current_thread_host_priority",
        "getpriority(PRIO_PROCESS, gettid()) on Linux; a thread QoS class on macOS",
    )
}
