//! Linux and macOS backend for the guest-fault seam.
//!
//! **Structural only: unverified and not implemented.** Every entry point returns
//! [`FaultError::Unsupported`](super::FaultError::Unsupported), so a build for those targets
//! fails honestly at the first call
//! rather than appearing to work. This mirrors [`crate::vm`]'s unix backend exactly, and for the same
//! reason: a seam whose unimplemented half returns `Ok` is worse than no seam.
//!
//! What has to be decided by measurement before this is written:
//!
//! * The POSIX mechanism is `sigaction(SIGSEGV, …, SA_SIGINFO)` reading `siginfo_t::si_addr`, and
//!   there is **no "continue execution" return value** — a handler that has fixed the mapping just
//!   returns, and the instruction is retried. That is the same effect, but the failure mode when the
//!   mapping was *not* fixed is an infinite signal loop rather than a second-chance crash.
//! * The read/write/execute distinction is not in `siginfo_t`. It has to be dug out of the
//!   machine context (`ucontext_t`'s `err` word on x86-64 Linux, `esr` on AArch64), which is
//!   architecture-specific in a way the Windows parameters are not.
//! * A handler must be async-signal-safe. Committing memory through `mmap` is, taking a
//!   `parking_lot` mutex is not, so the demand-paging design above this seam may need to change
//!   shape rather than just gain a backend.
//! * Linux ARM64 is the host where this matters least, because `ARCHITECTURE.md` §6 runs guest code
//!   natively there and there is no JIT code cache to be first in front of.

use super::{FaultError, FaultHandler, FaultRegistration, FaultResult, FaultStats};

/// This backend is structural.
pub(super) const AVAILABLE: bool = false;

/// The platform this backend was compiled for, for error messages.
fn platform() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "this platform"
    }
}

pub(super) fn install(_handler: FaultHandler, _context: usize) -> FaultResult<FaultRegistration> {
    Err(FaultError::Unsupported { platform: platform() })
}

pub(super) fn release(_slot: usize) {}

pub(super) fn stats() -> FaultStats {
    FaultStats::default()
}
