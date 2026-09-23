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
    /// A Windows API failed; `code` is its `GetLastError`.
    #[error("`{operation}`: {api} failed with GetLastError {code}")]
    LastError {
        /// The seam operation that was called.
        operation: &'static str,
        /// The OS entry point that failed.
        api: &'static str,
        /// The raw `GetLastError` code.
        code: u32,
    },
    /// The calling thread asked to sample itself. Suspending the caller would never resume it.
    #[error("a thread cannot sample itself: suspending the caller would never resume it")]
    SampledItself,
}
