//! Typed errors for the sampler seam, in the same shape as the process seam's.

/// Result alias for the sampler seam.
pub type SamplerResult<T> = Result<T, SamplerError>;

/// Everything the sampler seam can refuse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SamplerError {
    /// This backend does not implement the operation. `intended` names the mechanism the
    /// implementation is expected to use.
    #[error(
        "sampler operation `{operation}` is not implemented on {platform}: the intended \
         implementation is `{intended}`, and omni-platform's {platform} backend is structural only"
    )]
    Unsupported {
        /// The seam operation that was called.
        operation: &'static str,
        /// The mechanism the implementation is meant to use.
        intended: &'static str,
        /// The target the backend was compiled for.
        platform: &'static str,
    },
    /// A Windows API failed; `code` is its `GetLastError`. (Linux reports [`Errno`](Self::Errno),
    /// macOS [`Kern`](Self::Kern) or [`Errno`](Self::Errno).)
    #[error("`{operation}`: {api} failed with GetLastError {code}")]
    LastError {
        /// The seam operation that was called.
        operation: &'static str,
        /// The OS entry point that failed.
        api: &'static str,
        /// The raw `GetLastError` code.
        code: u32,
    },
    /// A POSIX call that reports through `errno` failed (Linux, macOS). A variant of its own for
    /// the process seam's reason: errno 13 is `EACCES`, Win32 error 13 is `ERROR_INVALID_DATA`.
    #[error("`{operation}`: {api} failed with errno {errno} ({})", std::io::Error::from_raw_os_error(*.errno))]
    Errno {
        /// The seam operation that was called.
        operation: &'static str,
        /// The OS entry point that failed.
        api: &'static str,
        /// The raw `errno` value, in the host's own numbering.
        errno: i32,
    },
    /// A Mach call failed (macOS); `code` is its `kern_return_t`.
    #[error("`{operation}`: {api} failed with kern_return_t {code}")]
    Kern {
        /// The seam operation that was called.
        operation: &'static str,
        /// The OS entry point that failed.
        api: &'static str,
        /// The raw `kern_return_t`.
        code: i32,
    },
    /// The thread did not answer the sampling signal in time (Linux): it had the signal blocked,
    /// or was not scheduled within the wait. Nothing is left pending on its behalf -- a late answer
    /// is recognised as stale and discarded by the handler.
    #[error("`{operation}`: the thread did not answer sampling signal {signal} within {waited_us} us")]
    NotAnswered {
        /// The seam operation that was called.
        operation: &'static str,
        /// The signal that was sent.
        signal: i32,
        /// How long the sampler waited, in microseconds.
        waited_us: u64,
    },
    /// The calling thread asked to sample itself. Suspending the caller would never resume it.
    #[error("a thread cannot sample itself: suspending the caller would never resume it")]
    SampledItself,
}
