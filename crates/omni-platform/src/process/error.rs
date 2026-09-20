//! Typed, diagnostic errors for the process seam.
//!
//! The same discipline as [`VmError`](crate::vm::VmError): every variant names the operation and
//! the values it failed with (Global Constraint 7), and there is deliberately no catch-all
//! `Other(String)`.

/// Result alias for every operation on the process seam.
pub type ProcessResult<T> = Result<T, ProcessError>;

/// Everything that can go wrong on the process seam.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProcessError {
    /// This backend does not implement the operation.
    ///
    /// Returned by the Linux and macOS backends, which are structural only. `intended` names the
    /// POSIX call the implementation is expected to make, so that the refusal says what the
    /// missing work *is* rather than only that it is missing.
    #[error(
        "process operation `{operation}` is not implemented on {platform}: the intended \
         implementation is `{intended}`, and omni-platform's {platform} backend is structural \
         only and has never been run (see docs/ARCHITECTURE.md \"Portability rule\")"
    )]
    Unsupported {
        /// The seam operation that was called, e.g. `"random_bytes"`.
        operation: &'static str,
        /// The POSIX call the implementation is meant to make, e.g. `"getrandom(2)"`.
        intended: &'static str,
        /// The target the backend was compiled for, e.g. `"linux"`.
        platform: &'static str,
    },

    /// A Windows API that reports through `NTSTATUS` failed.
    ///
    /// Kept separate from a `GetLastError` code because the two number spaces are different and
    /// rendering an `NTSTATUS` as a Win32 error is how a diagnostic becomes a wrong lead.
    #[error("`{operation}`: {api} failed with NTSTATUS {status:#010x}")]
    Status {
        /// The seam operation that was called.
        operation: &'static str,
        /// The OS entry point that failed.
        api: &'static str,
        /// The raw `NTSTATUS`.
        status: i32,
    },
}

impl ProcessError {
    /// True when this failure means "this backend has no implementation".
    ///
    /// Mirrors [`VmError::is_unsupported`](crate::vm::VmError::is_unsupported) so that a caller
    /// can distinguish "this target has never been built out" from "the OS said no" without
    /// matching every variant shape.
    #[must_use]
    pub fn is_unsupported(&self) -> bool {
        matches!(self, ProcessError::Unsupported { .. })
    }
}
