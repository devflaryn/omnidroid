//! Unix backend for the audio seam: every unix target except Linux, which has its own ALSA
//! backend in `linux.rs` (so this module is not compiled there).
//!
//! # Status: structural, not implemented
//!
//! **Nothing in this module has ever been run.** [`AudioOutput::open`] returns
//! [`AudioError::Unsupported`] naming the platform API it intends to reach for, so a macOS build
//! fails at the first stream rather than appearing to play.
//!
//! `AudioOutput` here is an **uninhabited** type, for the reason
//! [`window::unix`](crate::window) gives at length: `open` is the only way to get one and it
//! refuses, so every other operation's body is `match *self {}` — a statement the compiler proves
//! rather than a refusal no test could ever reach (VERIFICATION entry 12).
//!
//! # What implementing this involves
//!
//! * **Linux** is implemented, in `linux.rs`, on ALSA; this paragraph is the reasoning it started
//!   from. Linux has more than one candidate API and the choice is a runtime one. PipeWire, the
//!   sound server on most current desktops, also serves PulseAudio's protocol, so PulseAudio's
//!   asynchronous `pa_stream` API — the one with a "how much can I write" question,
//!   `pa_stream_writable_size`, and so the one that fits this seam's
//!   [`writable_frames`](super::AudioOutput::writable_frames) — reaches both. ALSA's
//!   `snd_pcm_open(3)` with `snd_pcm_avail_update` and `snd_pcm_wait` is the fallback with no
//!   server, and maps onto this seam almost call for call. **The format question is different
//!   there**: neither API has a single "mix format" that must be used, so "the device's own
//!   format" has to become "ask for float at the server's default rate and report what was
//!   granted" — which keeps the seam's contract (report, never convert here) but not its wording.
//! * **macOS** is Core Audio: an `AudioUnit` of subtype `kAudioUnitSubType_DefaultOutput`, whose
//!   stream format is readable (`kAudioUnitProperty_StreamFormat`) and whose canonical sample type
//!   is 32-bit float, so the `f32` contract should carry over unchanged (unverified). It is
//!   **callback-driven**, which this seam's pull shape is not: the render callback runs on Core
//!   Audio's real-time thread, so the backend needs a ring buffer between that callback and
//!   [`write`](super::AudioOutput::write), with `wait_writable` waiting on the ring's free space.
//!   That ring is the whole of the work, and its size is what `buffer_frames` would report.

use std::time::Duration;

use super::{AudioError, AudioResult, OutputFormat};

/// The structural unix output: **a type with no values**. See this module's header.
pub(super) enum AudioOutput {}

impl AudioOutput {
    /// The one reachable operation, and it refuses.
    pub(super) fn open(buffer_frames: u32) -> AudioResult<Self> {
        let _ = buffer_frames;
        Err(AudioError::Unsupported {
            operation: "open",
            intended: "pa_stream_new(3) or snd_pcm_open(3) on Linux, an AudioUnit of subtype \
                       kAudioUnitSubType_DefaultOutput on macOS",
            target: std::env::consts::OS,
        })
    }

    /// Unreachable: `open` above never produces an `AudioOutput`, so the compiler discharges this.
    pub(super) fn format(&self) -> OutputFormat {
        match *self {}
    }

    /// Unreachable, as [`AudioOutput::format`].
    pub(super) fn buffer_frames(&self) -> u32 {
        match *self {}
    }

    /// Unreachable, as [`AudioOutput::format`].
    pub(super) fn period_frames(&self) -> u32 {
        match *self {}
    }

    /// Unreachable, as [`AudioOutput::format`].
    pub(super) fn writable_frames(&self, operation: &'static str) -> AudioResult<u32> {
        let _ = operation;
        match *self {}
    }

    /// Unreachable, as [`AudioOutput::format`].
    pub(super) fn wait_writable(&self, timeout: Duration) -> AudioResult<u32> {
        let _ = timeout;
        match *self {}
    }

    /// Unreachable, as [`AudioOutput::format`].
    pub(super) fn write(&mut self, samples: &[f32], frames: u32) -> AudioResult<()> {
        let _ = (samples, frames);
        match *self {}
    }

    /// Unreachable, as [`AudioOutput::format`].
    pub(super) fn start(&mut self) -> AudioResult<()> {
        match *self {}
    }

    /// Unreachable, as [`AudioOutput::format`].
    pub(super) fn stop(&mut self) -> AudioResult<()> {
        match *self {}
    }
}
