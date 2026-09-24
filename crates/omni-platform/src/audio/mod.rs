//! Audio output: the platform seam.
//!
//! # What this module is
//!
//! One output stream on the host's **default** playback device, in **shared** mode, at the
//! device's **own** mix format, fed interleaved `f32` samples by a thread that waits for the device
//! to ask for them. Nothing else in the workspace may call an OS audio API (Global Constraint 4),
//! so this is the whole of it: `omni-android`'s `libaaudio.so` — the AAudio the guest's FMOD opens
//! — writes through an [`AudioOutput`] and reaches the host no other way.
//!
//! It is the same shape as [`window`](crate::window): a portable API, a Windows backend, and a
//! structural unix one, with the backend list fixed by the compiler. `mod.rs` calls exactly
//!
//! ```text
//! AudioOutput::open(buffer_frames: u32) -> AudioResult<AudioOutput>
//! AudioOutput::format(&self) -> OutputFormat
//! AudioOutput::buffer_frames(&self) -> u32
//! AudioOutput::period_frames(&self) -> u32
//! AudioOutput::writable_frames(&self, operation) -> AudioResult<u32>
//! AudioOutput::wait_writable(&self, timeout: Duration) -> AudioResult<u32>
//! AudioOutput::write(&mut self, samples: &[f32], frames: u32) -> AudioResult<()>
//! AudioOutput::start(&mut self) -> AudioResult<()>
//! AudioOutput::stop(&mut self) -> AudioResult<()>
//! ```
//!
//! so a backend that is missing one, or whose signature has drifted, does not build for that
//! target.
//!
//! # The device's format, not the guest's, and nothing converts between them
//!
//! [`AudioOutput::open`] takes no sample rate and no channel count. It opens the device at the
//! format the host already mixes at and **reports** it ([`AudioOutput::format`]); the caller adapts.
//! That is possible because of how AAudio is specified: a stream is *requested* with a rate and a
//! channel count, and `AAudioStream_getSampleRate` / `getChannelCount` report what it was actually
//! *given*, which may differ. So `libaaudio.so` can answer the guest with the host's numbers and no
//! resampler has to exist anywhere. That FMOD reads those numbers back and renders at them, rather
//! than assuming the request was honoured, is **ASSUMED** from AAudio's contract and has not been
//! observed yet.
//!
//! The samples are `f32` and only `f32` for the same reason. A shared-mode stream must be opened at
//! the mix format, and on Windows the audio engine mixes in 32-bit float; a device reporting
//! anything else is refused by name ([`AudioError::FormatNotFloat`]) rather than converted, because a
//! conversion path no host this project runs on can reach would be a path nobody has run.
//!
//! # Why the caller waits, and nothing calls back
//!
//! The runtime owns its threads, for the reason [`window`](crate::window)'s "Why polling" gives:
//! a host API that calls back on a thread of its own would be running guest-facing code on a
//! thread the runtime did not create and cannot name to the guest. So the shape here is a **pull**:
//! the thread that feeds the device calls [`AudioOutput::wait_writable`], which blocks until the
//! device signals that it has consumed a period (or a timeout passes), and then writes what
//! [`AudioOutput::writable_frames`] says fits. AAudio's data callback is built on top of that loop
//! in `libaaudio.so`, on a thread the runtime starts.
//!
//! # Writes are never truncated
//!
//! A write larger than the free space is [`AudioError::TooManyFrames`] and writes **nothing**.
//! Playing the part that fits and dropping the rest would put the guest's idea of how much it has
//! played ahead of the truth by an amount nobody records — a clock drift with no error attached.
//!
//! # Start after writing
//!
//! A stream started with nothing queued underruns on its first period. Microsoft's own WASAPI
//! render example fills the buffer before `IAudioClient::Start` for that reason, and so should the
//! caller here: write at least a period (silence is fine) before [`AudioOutput::start`]. This seam
//! does not do it on the caller's behalf, because the caller is the one that knows what the first
//! samples should be.
//!
//! # Measured on the development host
//!
//! `tests/audio_live.rs`, Windows 11 x86-64: the default device mixes at **48,000 Hz, 2
//! channels**, float; a request for 4,800 frames got exactly 4,800 and one for 24,000 got 24,000;
//! the period is **480 frames** (the engine's 10 ms). After `start`, the first period's worth of
//! space was free about 4 ms later, the buffer then drained at 47,966 frames/s over a 200 ms
//! window, and a waiter on a refilled buffer was woken with 480 frames free about 9 ms after it
//! began waiting. A stream that has not been started neither drains nor signals: a 100 ms wait on
//! one ran to its timeout and returned 0.

use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use std::time::Duration;

mod error;

pub use error::{AudioError, AudioResult};

// The backend modules are **private**, for the reason `vm::mod` records: a `pub mod windows` is a
// public surface no other crate can name without writing `#[cfg(target_os = "windows")]` itself,
// which Global Constraint 4 forbids everywhere but here.
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as backend;

// A unix that is neither macOS (Core Audio) nor Linux (ALSA): the structural body.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
mod unix;
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
use unix as backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;
/// How often the Linux backend has recovered the stream rather than failed. See
/// [`AudioOutput::recoveries`].
#[cfg(target_os = "linux")]
pub use linux::Recoveries;

/// The shape of the samples an output takes: always interleaved `f32`.
///
/// This is what the **device** runs at, as reported by the host — not what anybody asked for. See
/// this module's "The device's format, not the guest's".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFormat {
    /// Frames per second. Never zero.
    pub sample_rate: u32,
    /// Samples per frame, interleaved in the host's speaker order (front left, front right, then
    /// centre, LFE, back left, back right, … for layouts wider than stereo). Never zero.
    pub channels: u16,
}

/// An output stream on the host's default render device, in shared mode, at the device's own mix
/// format.
///
/// # Thread affinity
///
/// `Send`, because it is opened by whoever sets audio up and then driven by a thread of its own —
/// the one that loops on [`AudioOutput::wait_writable`]. **Not `Sync`**: WASAPI's buffer is handed
/// out and returned in two calls (`GetBuffer`, `ReleaseBuffer`), and two threads writing through
/// one stream would interleave them. [`AudioOutput::write`] taking `&mut self` is what serialises
/// that, and it only can if the type cannot be shared. The marker is here rather than left to the
/// backend so that the property is the same on every target — the structural unix backend's type
/// has no fields at all, and would otherwise be `Sync` by accident.
///
/// # Lifetime
///
/// Dropping it stops the stream if it is running and releases everything it holds.
pub struct AudioOutput {
    inner: backend::AudioOutput,
    /// `Cell` is `Send` and not `Sync`, which is exactly the property wanted. See "Thread
    /// affinity".
    _not_sync: PhantomData<Cell<()>>,
}

impl AudioOutput {
    /// Open the host's default render endpoint in shared mode at its mix format, event-driven,
    /// asking for a buffer of about `buffer_frames` frames. **Not started.**
    ///
    /// The buffer size is a request: the device may give more, and [`AudioOutput::buffer_frames`]
    /// reports what it gave. Zero asks for the smallest buffer the host will give.
    ///
    /// # Errors
    ///
    /// [`AudioError::NoDevice`] when the host has no default playback device;
    /// [`AudioError::FormatNotFloat`] when the device's mix format is not 32-bit float;
    /// [`AudioError::Os`] when a host call fails; [`AudioError::Unsupported`] on the structural
    /// backends.
    pub fn open(buffer_frames: u32) -> AudioResult<AudioOutput> {
        Ok(AudioOutput {
            inner: backend::AudioOutput::open(buffer_frames)?,
            _not_sync: PhantomData,
        })
    }

    /// The device's mix format — the sample rate and channel count the host really runs at.
    ///
    /// Fixed for the life of the stream.
    #[must_use]
    pub fn format(&self) -> OutputFormat {
        self.inner.format()
    }

    /// The host buffer's size in frames: the most that can ever be queued at once.
    #[must_use]
    pub fn buffer_frames(&self) -> u32 {
        self.inner.buffer_frames()
    }

    /// The device period in frames: how much the host consumes between two wake-ups of
    /// [`AudioOutput::wait_writable`], at the device's rate.
    #[must_use]
    pub fn period_frames(&self) -> u32 {
        self.inner.period_frames()
    }

    /// Frames that can be written now without overrunning: the buffer's size minus what is still
    /// queued in it.
    ///
    /// Asked of the host each time rather than tracked here, because the host is the one draining
    /// the buffer and only it knows how far it has got.
    ///
    /// # Errors
    ///
    /// [`AudioError::Os`] if the host could not say — on Windows,
    /// `AUDCLNT_E_DEVICE_INVALIDATED` when the device has gone away, after which the stream must be
    /// reopened.
    pub fn writable_frames(&self) -> AudioResult<u32> {
        self.inner.writable_frames("writable_frames")
    }

    /// Block until the device signals that it wants data or `timeout` passes, then return
    /// [`AudioOutput::writable_frames`].
    ///
    /// **A timeout is not an error**: it returns whatever is writable, which may be zero. A stream
    /// that has not been started does not signal, so waiting on one always runs to the timeout.
    ///
    /// # Errors
    ///
    /// [`AudioError::Os`] if the wait itself failed or the host could not report the free space.
    pub fn wait_writable(&self, timeout: Duration) -> AudioResult<u32> {
        self.inner.wait_writable(timeout)
    }

    /// Append interleaved samples to the host buffer.
    ///
    /// `samples.len()` must be a whole number of frames, and the frames must fit in
    /// [`AudioOutput::writable_frames`]. An empty slice writes nothing and succeeds.
    ///
    /// # Errors
    ///
    /// [`AudioError::TooManyFrames`] when the write is larger than the free space — **and nothing
    /// is written** (see this module's "Writes are never truncated"); [`AudioError::Os`] when the
    /// host refused the buffer.
    ///
    /// # Panics
    ///
    /// When `samples.len()` is not a multiple of [`OutputFormat::channels`]. A partial frame is not
    /// something the host can be handed, and there is no correct place to put the samples it has:
    /// it is a caller computing its lengths from a different channel count than the stream's, and
    /// nothing downstream of that is worth running.
    pub fn write(&mut self, samples: &[f32]) -> AudioResult<()> {
        let channels = self.inner.format().channels;
        let Some(frames) = whole_frames(samples.len(), channels) else {
            panic!(
                "AudioOutput::write was handed {} samples, which is not a whole number of \
                 {channels}-channel frames",
                samples.len()
            );
        };
        if frames == 0 {
            return Ok(());
        }
        let writable = self.inner.writable_frames("write")?;
        // More than `u32::MAX` frames is more than any host buffer holds, so it saturates into a
        // count that is still larger than `writable` and is refused below like any other.
        let frames = u32::try_from(frames).unwrap_or(u32::MAX);
        if frames > writable {
            return Err(AudioError::TooManyFrames { operation: "write", frames, writable });
        }
        self.inner.write(samples, frames)
    }

    /// Start the device consuming the buffer. Starting a running stream does nothing.
    ///
    /// # Errors
    ///
    /// [`AudioError::Os`] if the host refused.
    pub fn start(&mut self) -> AudioResult<()> {
        self.inner.start()
    }

    /// Stop the device consuming the buffer. What is still queued stays queued, and plays when the
    /// stream is started again. Stopping a stopped stream does nothing.
    ///
    /// # Errors
    ///
    /// [`AudioError::Os`] if the host refused.
    pub fn stop(&mut self) -> AudioResult<()> {
        self.inner.stop()
    }

    /// **Linux only**: how many underruns (`-EPIPE`) and suspends (`-ESTRPIPE`) ALSA reported and
    /// the backend recovered from, since `open`. Recovered rather than returned, so that a
    /// stream keeps playing as it does on Windows -- and counted, so that recovering is never the
    /// same as hiding. WASAPI's shared mode has no equivalent count, which is why this is not
    /// part of the portable surface.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn recoveries(&self) -> Recoveries {
        self.inner.recoveries()
    }
}

impl fmt::Debug for AudioOutput {
    /// The stream's fixed shape. The free space is not printed: it is a host query that can fail,
    /// and a `Debug` that makes one is a `Debug` with a side channel.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioOutput")
            .field("format", &self.inner.format())
            .field("buffer_frames", &self.inner.buffer_frames())
            .field("period_frames", &self.inner.period_frames())
            .finish_non_exhaustive()
    }
}

/// The number of whole frames `samples` interleaved samples make, or `None` when the last one is
/// partial.
///
/// `channels` is never zero here: every backend's `format` comes from a mix format that was refused
/// if it had no channels.
fn whole_frames(samples: usize, channels: u16) -> Option<usize> {
    let channels = usize::from(channels);
    (samples % channels == 0).then_some(samples / channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_frames_counts_frames_and_refuses_a_partial_one() {
        assert_eq!(whole_frames(0, 2), Some(0));
        assert_eq!(whole_frames(960, 2), Some(480));
        assert_eq!(whole_frames(961, 2), None, "half a stereo frame");
        assert_eq!(whole_frames(6 * 100, 6), Some(100), "5.1");
        assert_eq!(whole_frames(6 * 100 + 4, 6), None);
        assert_eq!(whole_frames(7, 1), Some(7), "mono takes any length");
    }

    /// `Send` and not `Sync`, checked by the compiler on every target — the second half by the
    /// ambiguity trick: if `AudioOutput` were `Sync`, both impls below would apply and the
    /// `some_item` path would not resolve.
    #[test]
    fn audio_output_is_send_and_not_sync() {
        fn assert_send<T: Send>() {}
        assert_send::<AudioOutput>();

        trait AmbiguousIfSync<A> {
            fn some_item() {}
        }
        impl<T: ?Sized> AmbiguousIfSync<()> for T {}
        impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}
        <AudioOutput as AmbiguousIfSync<_>>::some_item();
    }
}
