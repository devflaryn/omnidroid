//! Typed, diagnostic errors for the web-view seam.
//!
//! The same discipline as the [`window`](super::super::window) and [`audio`](super::super::audio)
//! seams: every variant names the operation and the values it failed with, and there is no
//! catch-all `Other(String)`.
//!
//! These are only the failures a call can report **synchronously**. The web view does most of its
//! work later, on its own thread — the browser environment and controller are created
//! asynchronously, and a navigation fails long after [`navigate`](super::WebView::navigate)
//! returned — so everything after `open` returns is reported as a
//! [`WebViewEvent`](super::WebViewEvent) instead.

/// Result alias for every operation on the web-view seam.
pub type WebViewResult<T> = Result<T, WebViewError>;

/// Everything a web-view call can report synchronously.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebViewError {
    /// This backend does not implement the operation.
    ///
    /// Returned by the unix backend, which is structural only. `intended` names the platform API
    /// the implementation is expected to reach for; the field names follow
    /// [`AudioError::Unsupported`](super::super::audio::AudioError::Unsupported).
    #[error(
        "web view operation `{operation}` is not implemented on {target}: the intended \
         implementation is `{intended}`, and omni-platform's {target} web view backend is \
         structural only and has never been run (see docs/ARCHITECTURE.md \"Portability rule\")"
    )]
    Unsupported {
        /// The seam operation that was called, e.g. `"open"`.
        operation: &'static str,
        /// The platform API the implementation is meant to reach for.
        intended: &'static str,
        /// The target the backend was compiled for, e.g. `"linux"`.
        target: &'static str,
    },

    /// The host has no usable browser runtime: on Windows, no Microsoft Edge WebView2 (Evergreen)
    /// runtime is registered, the one registered is too old, or its client DLL is not where the
    /// registration says. `detail` says which, naming the registry keys and paths looked at.
    #[error("no web view runtime: {detail}")]
    RuntimeMissing {
        /// What was looked for and what was found instead.
        detail: String,
    },

    /// A host call failed.
    ///
    /// `code` is an `HRESULT`. Calls that report through `GetLastError` or return a Win32 code
    /// (`CreateWindowExW`, `RegGetValueW`, `LoadLibraryExW`) are carried as
    /// `HRESULT_FROM_WIN32` of it (`0x8007xxxx`), so that every code here is on one scale and
    /// prints the way the SDK headers spell it — the convention the audio seam set.
    #[error("`{operation}`: {api} failed with HRESULT {code:#010x}")]
    Os {
        /// The seam operation that was called.
        operation: &'static str,
        /// The host entry point that failed.
        api: &'static str,
        /// The raw `HRESULT`, printed in hex.
        code: i32,
    },

    /// macOS: the web view needs the AppKit thread the window seam hands the main thread to, and
    /// there is none (`why` is the window seam's own account of it).
    #[error("`{operation}`: no AppKit main thread for a web view: {why}")]
    MainThreadUnavailable {
        /// The operation refused.
        operation: &'static str,
        /// Why the main thread is not serving AppKit.
        why: &'static str,
    },

    /// The web view's window has closed — by [`close`](super::WebView::close), or by the person
    /// closing it — so there is nothing left to send the command to.
    #[error("`{operation}`: the web view's window has closed")]
    Closed {
        /// The seam operation that was called.
        operation: &'static str,
    },

    /// An argument cannot be handed to the host as it is.
    ///
    /// **Not in the brief's API; added.** WebView2 takes every string NUL-terminated, so a title,
    /// URL or script with a NUL inside it would be **silently truncated** there — the
    /// silent-wrong-answer shape the window seam's `TitleHasInteriorNul` exists to refuse — and a
    /// zero-sized window, or one wider than `WM_SIZE` can report, cannot be made. Rejected before
    /// any host call, naming the argument and the offending value or position.
    #[error("`{operation}`: {argument} {detail}")]
    InvalidArgument {
        /// The seam operation that was called.
        operation: &'static str,
        /// The argument that was refused, e.g. `"url"` or `"init_script"`.
        argument: &'static str,
        /// What is wrong with it.
        detail: String,
    },
}

impl WebViewError {
    /// True when this failure means "this backend has no implementation".
    ///
    /// Mirrors [`AudioError::is_unsupported`](super::super::audio::AudioError::is_unsupported).
    #[must_use]
    pub fn is_unsupported(&self) -> bool {
        matches!(self, WebViewError::Unsupported { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_os_error_prints_its_hresult_as_the_headers_spell_it() {
        let error = WebViewError::Os { operation: "open", api: "CreateWindowExW", code: 0x8007_0578_u32 as i32 };
        let text = error.to_string();
        assert!(text.contains("0x80070578") && text.contains("CreateWindowExW"), "{text}");
        assert!(!error.is_unsupported());
    }

    #[test]
    fn only_unsupported_is_unsupported() {
        let unsupported =
            WebViewError::Unsupported { operation: "open", intended: "webkit_web_view_new(3)", target: "linux" };
        assert!(unsupported.is_unsupported());
        assert!(unsupported.to_string().contains("webkit_web_view_new(3)"), "{unsupported}");
        for other in [
            WebViewError::RuntimeMissing { detail: "x".into() },
            WebViewError::Closed { operation: "navigate" },
            WebViewError::InvalidArgument { operation: "open", argument: "url", detail: "is empty".into() },
        ] {
            assert!(!other.is_unsupported(), "{other:?}");
        }
    }
}
