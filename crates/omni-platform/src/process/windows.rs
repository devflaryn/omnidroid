//! Windows backend for the process seam.
//!
//! Two entry points, both thin, and each with one thing about it that is not obvious.

use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessorNumber;

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
