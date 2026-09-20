//! Shared unix body of the process seam, used by the [`linux`](super::linux) and
//! [`macos`](super::macos) backends.
//!
//! # Status: structural, not implemented
//!
//! **Nothing in this module has ever been run.** Each entry point returns
//! [`ProcessError::Unsupported`] naming the POSIX call it intends to make, so a Linux or macOS
//! build fails at the first call, immediately and with a message that says what to write, rather
//! than appearing to work.
//!
//! This is the same deliberate reading of Global Constraint 1 that [`crate::vm::unix`] records: an
//! unverified body misbehaves silently where a typed error fails visibly. It matters more here
//! than it looks, because both of these calls have a *believable* wrong answer available —
//! `random_bytes` could fill with a pseudo-random sequence and `current_cpu` could return 0, and
//! each would pass every test that only checked the call returned.
//!
//! # What implementing this involves
//!
//! Neither is hard; both have a decision in them that must be made by reading, not guessing.

use super::{ProcessError, ProcessResult};

/// The platform this backend was compiled for, for error messages.
fn platform() -> &'static str {
    std::env::consts::OS
}

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
pub(super) fn current_cpu() -> ProcessResult<u32> {
    unsupported("current_cpu", "sched_getcpu(3) on Linux; macOS has no supported equivalent")
}
