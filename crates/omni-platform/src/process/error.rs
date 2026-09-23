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

    /// A Windows API that reports through `GetLastError` failed.
    ///
    /// The **other** number space, and it is a separate variant for the reason
    /// [`Status`](ProcessError::Status) gives: `NTSTATUS 0xC0000008` and Win32 error 8 are
    /// different failures with the same digits, and rendering one as the other sends a reader
    /// after the wrong thing three thousand initializers deep.
    #[error("`{operation}`: {api} failed with GetLastError {code}")]
    LastError {
        /// The seam operation that was called.
        operation: &'static str,
        /// The OS entry point that failed.
        api: &'static str,
        /// The raw `GetLastError` code.
        code: u32,
    },

    /// A POSIX call that reports through `errno` failed.
    ///
    /// The **third** number space, and a variant of its own for the reason
    /// [`Status`](ProcessError::Status) and [`LastError`](ProcessError::LastError) are two: `errno`
    /// 13 is `EACCES`, Win32 error 13 is `ERROR_INVALID_DATA`, and a message that rendered one as
    /// the other would name the wrong failure. Added by the Linux backend, whose calls
    /// (`getrandom`, `sched_getcpu`, `setpriority`, `clock_gettime`) all report this way.
    #[error("`{operation}`: {api} failed with errno {errno} ({})", std::io::Error::from_raw_os_error(*.errno))]
    Errno {
        /// The seam operation that was called.
        operation: &'static str,
        /// The OS entry point that failed.
        api: &'static str,
        /// The raw `errno` value, in the host's own numbering.
        errno: i32,
    },

    /// A quantity the standard library reports, which it could not determine.
    ///
    /// **Not a catch-all**, and the distinction from the two Windows variants above is the point:
    /// those name an OS entry point and a number space, and this one names a `std` query that is
    /// documented as being able to fail — `available_parallelism` does, on a target with no such
    /// notion or with the permission to ask withheld. It exists because the alternative was
    /// substituting an answer, which is what the phase-3 review's finding M7 was about.
    #[error("`{operation}`: the standard library could not determine it: {detail}")]
    Indeterminate {
        /// The seam operation that was called.
        operation: &'static str,
        /// What the standard library said.
        detail: String,
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
