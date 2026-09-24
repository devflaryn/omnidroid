//! Typed, diagnostic errors for the audio seam.
//!
//! The same discipline as [`WindowError`](crate::window::WindowError) and
//! [`VmError`](crate::vm::VmError): every variant names the operation and the values it failed with
//! (Global Constraint 7), and there is deliberately no catch-all `Other(String)`.
//!
//! One thing is specific to this seam. WASAPI reports every failure as an `HRESULT`, and a handful
//! of those have a meaning a caller can act on without looking the number up — there is no device
//! at all, the device mixes in a sample type this seam will not convert — so those get a variant of
//! their own and everything else is [`AudioError::Os`] with the raw code. A caller that has to turn
//! this into a guest error code (AAudio's `AAUDIO_ERROR_*`) matches the named ones and treats the
//! rest as the host failing.

/// Result alias for every operation on the audio seam.
pub type AudioResult<T> = Result<T, AudioError>;

/// Everything that can go wrong on the audio seam.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    /// This backend does not implement the operation.
    ///
    /// Returned by the unix backend, which is structural only. `intended` names the platform API
    /// the implementation is expected to reach for, so the refusal says what the missing work *is*
    /// rather than only that it is missing — the shape
    /// [`WindowError::Unsupported`](crate::window::WindowError::Unsupported) uses. (The field that
    /// names the target is `target` here where the older seams say `platform`; the audio seam's
    /// consumer was written against that spelling.)
    #[error(
        "audio operation `{operation}` is not implemented on {target}: the intended \
         implementation is `{intended}`, and omni-platform's {target} audio backend is \
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

    /// A host audio call failed.
    ///
    /// `code` is an `HRESULT` on Windows. Most of this seam is COM and reports one directly; the
    /// two kernel calls it makes (`CreateEventW`, `WaitForSingleObject`) report through
    /// `GetLastError` instead, and are carried as `HRESULT_FROM_WIN32` of that code
    /// (`0x8007xxxx`), so that every code in this variant is on one scale and prints the way the
    /// SDK headers spell it.
    ///
    /// The one worth knowing by sight is `AUDCLNT_E_DEVICE_INVALIDATED` (`0x88890004`): the
    /// endpoint went away under a running stream — unplugged headphones, a disabled device, the
    /// default device changing to another one. The stream is dead after that and has to be
    /// reopened; nothing on it recovers.
    #[error("`{operation}`: {api} failed with HRESULT {code:#010x}")]
    Os {
        /// The seam operation that was called.
        operation: &'static str,
        /// The OS entry point that failed, e.g. `"IAudioClient::Initialize"`.
        api: &'static str,
        /// The raw `HRESULT`. Negative, as every failing `HRESULT` is; printed in hex.
        code: i32,
    },

    /// The host has no default audio output device.
    ///
    /// On Windows, `IMMDeviceEnumerator::GetDefaultAudioEndpoint` answered `E_NOTFOUND` — which
    /// `mmdeviceapi.h` defines as `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`, `0x80070490`, and which
    /// is what a machine with every playback device disabled or unplugged reports. A named variant
    /// rather than an [`AudioError::Os`] because it is not the host *failing*: it is a normal state
    /// of a desktop, and the runtime's right answer to it (run silent) differs from its answer to a
    /// broken audio stack.
    #[error("`{operation}`: the host has no default audio output device")]
    NoDevice {
        /// The seam operation that was called.
        operation: &'static str,
    },

    /// The device's mix format is not interleaved 32-bit IEEE float, the only sample shape this
    /// seam writes.
    ///
    /// **No conversion is attempted, on purpose.** A shared-mode stream must be opened at the
    /// device's own mix format, and on every Windows since Vista that format is float, because the
    /// audio engine mixes in float. A device that reports anything else is outside what this seam
    /// has been built or run against, and converting to it here would be a code path no host this
    /// project runs on can exercise — so it is refused, naming what the device said.
    ///
    /// `tag` is the format's `wFormatTag` (`0x0003` is `WAVE_FORMAT_IEEE_FLOAT`, `0xFFFE` is
    /// `WAVE_FORMAT_EXTENSIBLE`, whose real sample type is in a `SubFormat` GUID after the header)
    /// and `bits` its `wBitsPerSample`. The same refusal covers a format that says float but whose
    /// frames do not add up — zero channels, a zero rate, a block alignment other than four bytes
    /// per channel, an extensible header too short to carry its `SubFormat` — because writing into
    /// such a buffer would divide by zero or copy past the end of what the device handed over.
    #[error(
        "`{operation}`: the device's mix format (wFormatTag {tag:#06x}, {bits} bits per sample) \
         is not interleaved 32-bit IEEE float, the only sample shape this seam writes; no \
         conversion is attempted"
    )]
    FormatNotFloat {
        /// The seam operation that was called.
        operation: &'static str,
        /// The format's `wFormatTag`.
        tag: u16,
        /// The format's `wBitsPerSample`.
        bits: u16,
    },

    /// An ALSA call failed (the Linux backend, `src/audio/linux.rs`).
    ///
    /// Its own variant rather than [`AudioError::Os`], whose `code` is an `HRESULT` and prints as
    /// one: ALSA reports a negative errno, and `0xffffffe0` is not how anyone searches for
    /// `EPIPE`. `errno` is the positive value (`32` for `EPIPE`) and `description` is
    /// `snd_strerror`'s text for it, which also covers ALSA's own codes above the errno range.
    /// An xrun (`EPIPE`) or a suspend (`ESTRPIPE`) that the backend recovered from never becomes
    /// this error -- it is counted instead -- so seeing one of those here means the recovery
    /// itself failed, and `api` names the recovery call.
    #[error("`{operation}`: {api} failed with errno {errno} ({description})")]
    Alsa {
        /// The seam operation that was called.
        operation: &'static str,
        /// The ALSA entry point that failed, e.g. `"snd_pcm_writei"`.
        api: &'static str,
        /// The errno, positive.
        errno: i32,
        /// `snd_strerror`'s text for it.
        description: String,
    },

    /// A write was larger than the free space in the host buffer.
    ///
    /// **Nothing was written.** Writing the part that fits and dropping the rest would be a silent
    /// truncation — the guest's clock would run ahead of what was actually played — so the whole
    /// write is refused and the caller decides: wait with
    /// [`wait_writable`](super::AudioOutput::wait_writable), or write less.
    #[error(
        "`{operation}`: {frames} frames were written but only {writable} fit in the host buffer; \
         nothing was written"
    )]
    TooManyFrames {
        /// The seam operation that was called.
        operation: &'static str,
        /// The frames the caller tried to write (saturated at `u32::MAX`).
        frames: u32,
        /// The frames that were free at the time.
        writable: u32,
    },

    /// A Core Audio call failed. macOS only: Core Audio reports an `OSStatus`, which is often a
    /// four-character code (`'!fmt'`, `'nope'`) rather than a number, so both spellings are
    /// printed.
    #[error("`{operation}`: {api} failed with OSStatus {status} ({})", fourcc(*status))]
    OsStatus {
        /// The seam operation that was called.
        operation: &'static str,
        /// The Core Audio entry point that failed.
        api: &'static str,
        /// The raw `OSStatus`.
        status: i32,
    },

    /// The default device's stream format names no usable sample rate or channel count (a rate
    /// below 1 Hz or past `u32`, no channels, or more than `u16::MAX`). macOS only.
    #[error(
        "`{operation}`: the default output device reports {sample_rate} Hz and {channels} \
         channel(s), which is not a stream this seam can describe"
    )]
    DeviceFormatUnusable {
        /// The seam operation that was called.
        operation: &'static str,
        /// The reported rate, truncated to whole hertz.
        sample_rate: u64,
        /// The reported channel count.
        channels: u32,
    },
}

/// `status` as `'abcd'` when its four bytes are printable ASCII, as Core Audio's codes are, and as
/// `-` when it is a plain number.
fn fourcc(status: i32) -> String {
    let bytes = status.to_be_bytes();
    if bytes.iter().all(|b| (0x20..0x7F).contains(b)) {
        format!("'{}'", bytes.iter().map(|&b| char::from(b)).collect::<String>())
    } else {
        "-".to_owned()
    }
}

impl AudioError {
    /// True when this failure means "this backend has no implementation".
    ///
    /// Mirrors [`WindowError::is_unsupported`](crate::window::WindowError::is_unsupported), so that a
    /// caller can tell "this target has never been built out" from "the host said no" without
    /// matching every variant shape.
    #[must_use]
    pub fn is_unsupported(&self) -> bool {
        matches!(self, AudioError::Unsupported { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The number is printed the way the SDK headers spell it — a negative `i32` as its unsigned
    /// hex — because that is the only form anyone can search for.
    #[test]
    fn an_os_error_prints_its_hresult_as_the_headers_spell_it() {
        let error = AudioError::Os {
            operation: "writable_frames",
            api: "IAudioClient::GetCurrentPadding",
            code: 0x8889_0004_u32 as i32,
        };
        let text = error.to_string();
        assert!(text.contains("0x88890004"), "{text}");
        assert!(text.contains("IAudioClient::GetCurrentPadding"), "{text}");
        assert!(!error.is_unsupported());
    }

    #[test]
    fn only_unsupported_is_unsupported() {
        let unsupported = AudioError::Unsupported {
            operation: "open",
            intended: "snd_pcm_open(3)",
            target: "linux",
        };
        assert!(unsupported.is_unsupported());
        assert!(unsupported.to_string().contains("snd_pcm_open(3)"), "{unsupported}");
        for other in [
            AudioError::NoDevice { operation: "open" },
            AudioError::FormatNotFloat { operation: "open", tag: 1, bits: 16 },
            AudioError::TooManyFrames { operation: "write", frames: 2, writable: 1 },
        ] {
            assert!(!other.is_unsupported(), "{other:?}");
        }
    }
}
