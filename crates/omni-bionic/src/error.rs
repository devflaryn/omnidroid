//! Error types. Two layers:
//!
//! * [`Fault`] (in [`crate::memory`]) — a guest memory access failed at an address.
//! * [`BionicError`] — a function-level failure that carries a **name**: a `_chk` overflow, a
//!   requested-but-unimplementable function, an invalid argument. Rule: never return a
//!   plausible wrong answer; name the function or the check that cannot be honoured.

use crate::memory::Fault;

/// A function-level failure. `Display` always names the function or check responsible, so a
/// log line from a returned error is actionable without a backtrace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BionicError {
    /// A guest memory access failed. `0` is the faulting address.
    Memory(Fault),
    /// A FORTIFY `_chk` function detected an overflow (destination too small, or a source
    /// length beyond its bound). The string names the check, e.g. `"__memcpy_chk"`.
    CheckFailed(&'static str),
    /// The function cannot be implemented correctly in this crate and refuses to guess.
    /// The string names the function. Never a plausible stub.
    Unimplemented(&'static str),
    /// The arguments do not form a valid call (e.g. `wmemchr` with a null pointer and a
    /// nonzero count). The string names the function.
    InvalidArgument(&'static str),
}

impl core::fmt::Display for BionicError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BionicError::Memory(fault) => write!(f, "guest memory fault at {:#x}", fault.0),
            BionicError::CheckFailed(name) => write!(f, "{name}: fortify check failed"),
            BionicError::Unimplemented(name) => {
                write!(f, "{name}: not implementable in omni-bionic (no plausible stub)")
            }
            BionicError::InvalidArgument(name) => write!(f, "{name}: invalid argument"),
        }
    }
}

impl std::error::Error for BionicError {}

impl From<Fault> for BionicError {
    fn from(fault: Fault) -> Self {
        BionicError::Memory(fault)
    }
}

/// Shorthand for functions whose only failure mode is memory or a named check.
pub type BionicResult<T> = Result<T, BionicError>;
