//! Windows backend for the process seam.
//!
//! Two entry points, both thin, and each with one thing about it that is not obvious.

use std::time::Duration;

use windows_sys::Win32::Foundation::{FILETIME, GetLastError};
use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessorNumber, GetCurrentThread, GetProcessTimes,
    GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_BELOW_NORMAL,
    THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL,
};

use super::{ProcessError, ProcessResult};

/// `BCryptGenRandom(NULL, .., BCRYPT_USE_SYSTEM_PREFERRED_RNG)`.
///
/// The system-preferred RNG rather than an algorithm handle this module would have to open, hold
/// and close: passing the flag lets `bcrypt.dll` use the process-wide provider, which is what
/// every other consumer on the machine uses and what `RtlGenRandom` forwards to. It is documented
/// available from Windows Vista and is the call the Rust standard library itself makes.
///
/// `cbBuffer` is a `u32`, and a slice longer than `u32::MAX` is therefore filled in chunks rather
/// than truncated. A truncating cast here would report success having filled 0 bytes for a 4 GiB
/// request, which is the silent-wrong-answer shape: the caller is `arc4random_buf`, and a buffer
/// of zeroes that was supposed to be entropy is the single worst value to return.
pub(super) fn random_bytes(out: &mut [u8]) -> ProcessResult<()> {
    for chunk in out.chunks_mut(u32::MAX as usize) {
        // The cast cannot truncate: `chunks_mut` bounds the length by `u32::MAX`.
        let len = chunk.len() as u32;
        // SAFETY: `BCryptGenRandom` writes exactly `cbBuffer` bytes at `pbBuffer` and reads
        // nothing. `chunk` is a live, uniquely-borrowed slice of at least `len` bytes, and `len`
        // is its own length. A null algorithm handle is required — not merely permitted — by
        // `BCRYPT_USE_SYSTEM_PREFERRED_RNG`.
        let status = unsafe {
            BCryptGenRandom(core::ptr::null_mut(), chunk.as_mut_ptr(), len, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        };
        if status < 0 {
            return Err(ProcessError::Status {
                operation: "random_bytes",
                api: "BCryptGenRandom",
                status,
            });
        }
    }
    Ok(())
}

/// `GetCurrentProcessorNumber()`.
///
/// Cannot fail and returns no error code, so this is infallible on Windows — the `Result` is the
/// seam's shape, not this backend's need for one.
///
/// It reports a processor number **within the calling thread's processor group**, and a machine
/// with more than 64 logical processors has more than one group. That makes the value unique per
/// core only up to 64 cores, which is fine for its one legitimate use (a shard index) and would
/// not be fine for anything that assumed uniqueness. `GetCurrentProcessorNumberEx` returns the
/// group as well and is the call to reach for if that ever matters; it is not reached for now,
/// because `sched_getcpu` has no group concept either and widening past what the guest symbol can
/// express would invent a distinction the guest cannot see.
pub(super) fn current_cpu() -> ProcessResult<u32> {
    // SAFETY: takes no arguments, touches no memory, and cannot fail.
    Ok(unsafe { GetCurrentProcessorNumber() })
}

/// The Windows priority level, within the normal priority class, a clamped nice value maps to.
/// See [`super::set_current_thread_nice`] for the table and why it stops short of
/// `TIME_CRITICAL`.
fn priority_for_nice(nice: i32) -> i32 {
    match nice {
        i32::MIN..=-11 => THREAD_PRIORITY_HIGHEST,
        -10..=-1 => THREAD_PRIORITY_ABOVE_NORMAL,
        0 => THREAD_PRIORITY_NORMAL,
        1..=9 => THREAD_PRIORITY_BELOW_NORMAL,
        _ => THREAD_PRIORITY_LOWEST,
    }
}

/// `SetThreadPriority(GetCurrentThread(), ..)`.
pub(super) fn set_current_thread_nice(nice: i32) -> ProcessResult<()> {
    // SAFETY: `GetCurrentThread` returns a pseudo-handle for the calling thread that needs no
    // close; `SetThreadPriority` reads only it and the level.
    let ok = unsafe { SetThreadPriority(GetCurrentThread(), priority_for_nice(nice)) };
    if ok == 0 {
        // SAFETY: no arguments; `SetThreadPriority` is the last call this thread made.
        let code = unsafe { GetLastError() };
        return Err(ProcessError::LastError {
            operation: "set_current_thread_nice",
            api: "SetThreadPriority",
            code,
        });
    }
    Ok(())
}

/// `GetThreadPriority(GetCurrentThread())`.
pub(super) fn current_thread_host_priority() -> ProcessResult<i32> {
    // SAFETY: the calling thread's pseudo-handle; only its priority is read.
    let level = unsafe { GetThreadPriority(GetCurrentThread()) };
    // `THREAD_PRIORITY_ERROR_RETURN` is `MAXLONG`.
    if level == i32::MAX {
        // SAFETY: no arguments; `GetThreadPriority` is the last call this thread made.
        let code = unsafe { GetLastError() };
        return Err(ProcessError::LastError {
            operation: "current_thread_host_priority",
            api: "GetThreadPriority",
            code,
        });
    }
    Ok(level)
}

/// `GetProcessTimes(GetCurrentProcess(), ..)`, kernel time **plus** user time.
///
/// Three things about this call are not obvious, and each of them is a way to get a plausible
/// wrong number out of it.
///
/// **The pseudo-handle is not a handle.** `GetCurrentProcess()` returns `(HANDLE)-1`, a constant
/// that every process-scoped API resolves against the caller. It needs no `CloseHandle` and
/// costs no OS call; opening a real handle to this process to pass here would be a handle leak
/// waiting for the first early return.
///
/// **A `FILETIME` here is not a date.** The creation and exit fields are absolute times since
/// 1601, and the kernel and user fields are *durations* in the same 100-nanosecond unit. Reading
/// them as one kind or the other is the silent-wrong-answer shape, so only the two durations are
/// read and the two absolute times are discarded — they are written by the API whether or not
/// this function wants them, so they are received into locals rather than passed as null.
///
/// **The two halves are assembled rather than transmuted.** `FILETIME` is two `u32`s and is
/// documented as not necessarily 8-byte aligned, so casting one to a `u64` is undefined
/// behaviour on a struct the OS filled. Shift and OR instead.
///
/// The resolution is the scheduler's accounting quantum, which the seam's own documentation
/// records: this reports zero for a process that has run for less than one tick.
pub(super) fn cpu_time() -> ProcessResult<Duration> {
    let mut creation = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: `GetProcessTimes` writes four `FILETIME`s at the four pointers and reads only the
    // handle. All four are live, uniquely-borrowed locals of exactly that type. The current-process
    // pseudo-handle is always valid for the calling process and must not be closed.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if ok == 0 {
        // SAFETY: no arguments, no memory, and the call above is the last one this thread made.
        let code = unsafe { GetLastError() };
        return Err(ProcessError::LastError {
            operation: "cpu_time",
            api: "GetProcessTimes",
            code,
        });
    }
    let ticks = filetime_ticks(kernel).saturating_add(filetime_ticks(user));
    Ok(hundred_nanos(ticks))
}

/// A `FILETIME` read as a duration in 100-nanosecond units.
fn filetime_ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// `ticks` 100-nanosecond units as a [`Duration`].
///
/// Split rather than `Duration::from_nanos(ticks * 100)`, because that multiply overflows a `u64`
/// at about 58,000 years of accumulated CPU time — reachable by a machine with enough cores only
/// in principle, but a wrap would report a *small* duration for a huge one, which is the wrong
/// direction to be wrong in silently.
fn hundred_nanos(ticks: u64) -> Duration {
    const PER_SECOND: u64 = 10_000_000;
    Duration::new(ticks / PER_SECOND, ((ticks % PER_SECOND) * 100) as u32)
}

#[cfg(test)]
mod nice_tests {
    use super::*;

    /// The table in `set_current_thread_nice`'s documentation, at Android's own named values and
    /// at each tier's edges.
    #[test]
    fn android_priorities_land_in_the_documented_windows_levels() {
        for (nice, level) in [
            (-20, THREAD_PRIORITY_HIGHEST),
            (-19, THREAD_PRIORITY_HIGHEST),
            (-16, THREAD_PRIORITY_HIGHEST),
            (-11, THREAD_PRIORITY_HIGHEST),
            (-10, THREAD_PRIORITY_ABOVE_NORMAL),
            (-4, THREAD_PRIORITY_ABOVE_NORMAL),
            (-1, THREAD_PRIORITY_ABOVE_NORMAL),
            (0, THREAD_PRIORITY_NORMAL),
            (1, THREAD_PRIORITY_BELOW_NORMAL),
            (9, THREAD_PRIORITY_BELOW_NORMAL),
            (10, THREAD_PRIORITY_LOWEST),
            (19, THREAD_PRIORITY_LOWEST),
        ] {
            assert_eq!(priority_for_nice(nice), level, "nice {nice}");
        }
    }

    /// Applied, and read back, on a thread of its own.
    #[test]
    fn a_nice_value_is_applied_to_the_calling_thread() {
        std::thread::spawn(|| {
            assert_eq!(current_thread_host_priority().expect("read"), THREAD_PRIORITY_NORMAL);
            set_current_thread_nice(-16).expect("applied");
            assert_eq!(current_thread_host_priority().expect("read"), THREAD_PRIORITY_HIGHEST);
            set_current_thread_nice(19).expect("applied");
            assert_eq!(current_thread_host_priority().expect("read"), THREAD_PRIORITY_LOWEST);
        })
        .join()
        .expect("the thread completes");
    }
}
