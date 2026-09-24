//! Typed, diagnostic errors for the window seam.
//!
//! The same discipline as [`VmError`](crate::vm::VmError) and
//! [`ProcessError`](crate::process::ProcessError): every variant names the operation and the
//! values it failed with (Global Constraint 7), and there is deliberately no catch-all
//! `Other(String)`.
//!
//! One thing is specific to this seam and worth stating once. A windowing API fails in two very
//! different ways — *the host cannot do this at all* (no display, no session, a backend that was
//! never written) and *you asked for something impossible* (a 0-pixel window, a title with a NUL
//! in the middle of it). The second kind is rejected here, before any OS call, with the offending
//! value in the message, because a `CreateWindowExW` that fails with `ERROR_INVALID_PARAMETER`
//! says nothing about which of its twelve arguments was the bad one.

/// Result alias for every operation on the window seam.
pub type WindowResult<T> = Result<T, WindowError>;

/// Everything that can go wrong on the window seam.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WindowError {
    /// This backend does not implement the operation.
    ///
    /// Returned by the Linux and macOS backends, which are structural only. `intended` names the
    /// platform API the implementation is expected to reach for, so that the refusal says what
    /// the missing work *is* rather than only that it is missing — the same shape
    /// [`ProcessError::Unsupported`](crate::process::ProcessError::Unsupported) uses.
    #[error(
        "window operation `{operation}` is not implemented on {platform}: the intended \
         implementation is `{intended}`, and omni-platform's {platform} window backend is \
         structural only and has never been run (see docs/ARCHITECTURE.md \"Portability rule\")"
    )]
    Unsupported {
        /// The seam operation that was called, e.g. `"create"`.
        operation: &'static str,
        /// The platform API the implementation is meant to reach for, e.g. `"xcb_create_window(3)"`.
        intended: &'static str,
        /// The target the backend was compiled for, e.g. `"linux"`.
        platform: &'static str,
    },

    /// A Windows API that reports through `GetLastError` failed.
    ///
    /// Carries the raw code rather than a rendered string, because the number is the only thing
    /// that can be looked up — and because `CreateWindowExW` and `RegisterClassW` fail with the
    /// same `ERROR_INVALID_PARAMETER` for a dozen unrelated reasons, so the `api` field is what
    /// makes the code actionable.
    #[error("`{operation}`: {api} failed with GetLastError {code}")]
    LastError {
        /// The seam operation that was called.
        operation: &'static str,
        /// The OS entry point that failed.
        api: &'static str,
        /// The raw `GetLastError` code.
        code: u32,
    },

    /// The requested client size is not one a window can have.
    ///
    /// Zero is rejected because a zero-extent swapchain is invalid in Vulkan and a zero-extent
    /// window is invalid on Win32, so the failure would otherwise surface two layers away from
    /// the caller that chose the number. The upper bound is 65,535 in each axis: `WM_SIZE`
    /// reports the client size as two 16-bit halves of its `LPARAM`, so a window wider than that
    /// could be created and could never report its own size correctly — and it is far past the
    /// 32,768-pixel `maxImageDimension2D` this host's GPU measured
    /// (`docs/research/graphics-spike.md` §3), so nothing could present to it anyway.
    #[error(
        "`{operation}` was asked for a {width}x{height} client area; each axis must be at least \
         1 and at most {max} (WM_SIZE reports the client size in 16-bit halves of its LPARAM)"
    )]
    SizeOutOfRange {
        /// The seam operation that was called.
        operation: &'static str,
        /// The rejected width, in physical pixels.
        width: u32,
        /// The rejected height, in physical pixels.
        height: u32,
        /// The largest value either axis may take.
        max: u32,
    },

    /// The window title contains a NUL character.
    ///
    /// Win32 takes a NUL-terminated UTF-16 string, so a title with an interior NUL would be
    /// **silently truncated** at that point. Truncation is the silent-wrong-answer shape this
    /// project refuses everywhere else, so it is an error here and the byte offset is reported.
    #[error("`{operation}`: the window title contains a NUL character at index {at}; Win32 titles are NUL-terminated and would be truncated there")]
    TitleHasInteriorNul {
        /// The seam operation that was called.
        operation: &'static str,
        /// The index of the first NUL in the title, in `char` positions.
        at: usize,
    },

    /// **There is no thread AppKit can run on.** macOS only: AppKit is main-thread-only, and the
    /// backend hands the main thread to it before `main` runs -- unless it declined to, for the
    /// reason `why` names (see `window/macos/main_thread.rs`). Refused rather than attempted,
    /// because a window created off the main thread throws and one waiting for an unserved main
    /// thread would hang.
    #[error("`{operation}`: no window can be created because the main thread is not serving AppKit: {why}")]
    MainThreadUnavailable {
        /// The seam operation that was called.
        operation: &'static str,
        /// Why the main thread was not handed to AppKit.
        why: &'static str,
    },

    /// An AppKit or CoreGraphics call refused. macOS only; `detail` is what the host said
    /// (a `CGError`, or what was missing), since these APIs have no single error code space.
    #[error("`{operation}`: {api} failed: {detail}")]
    AppKit {
        /// The seam operation that was called.
        operation: &'static str,
        /// The host entry point that failed.
        api: &'static str,
        /// What went wrong, as the host reported it.
        detail: String,
    },
}

impl WindowError {
    /// True when this failure means "this backend has no implementation".
    ///
    /// Mirrors [`VmError::is_unsupported`](crate::vm::VmError::is_unsupported) and
    /// [`ProcessError::is_unsupported`](crate::process::ProcessError::is_unsupported) so that a
    /// caller can tell "this target has never been built out" from "the OS said no" without
    /// matching every variant shape. A renderer uses it to decide between *refuse to start* and
    /// *report a host problem*, which are different messages to a user.
    #[must_use]
    pub fn is_unsupported(&self) -> bool {
        matches!(self, WindowError::Unsupported { .. })
    }
}
