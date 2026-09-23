//! Linux backend for the audio seam: ALSA (`libasound`), one playback PCM on `"default"`.
//!
//! # Why ALSA, and not PipeWire's own API
//!
//! On a current desktop `"default"` is PipeWire's ALSA plugin (`pcm.pipewire`,
//! `/usr/share/alsa/alsa.conf.d/50-pipewire.conf`), so this reaches the sound server; on a host
//! with no server it is whatever the distribution's `alsa.conf` names (`plug`/`dmix` over the
//! card), and the same calls reach the hardware. ALSA's PCM API is already **pull-shaped** --
//! `snd_pcm_avail`, `snd_pcm_wait`, `snd_pcm_writei` are this seam's `writable_frames`,
//! `wait_writable` and `write` almost call for call -- where PipeWire's `pw_stream` is callback
//! driven and would need a ring buffer between its process callback and
//! [`write`](super::AudioOutput::write) (the work the macOS backend has to do for Core Audio).
//! The plugin *is* that ring, already written and already maintained.
//!
//! The one thing measured against ALSA: it has **no "mix format" to read**. MEASURED on the
//! development host (`aplay --dump-hw-params -D default`), PipeWire's plugin offers
//! `RATE: [1 384000]` and `CHANNELS: [1 128]` -- any rate, any count, resampled and remixed by the
//! server -- and a card opened directly offers its own ranges with no default either. Left to
//! itself, `snd_pcm_hw_params` would choose the *first* value of each range (alsa-lib's
//! `snd_pcm_hw_params_choose`), which is 1 Hz mono. So "the device's own format" becomes a
//! **request**: [`PREFERRED_RATE`] and [`PREFERRED_CHANNELS`], asked for with
//! `snd_pcm_hw_params_set_rate_near`/`set_channels_near`, and whatever the host granted is read back
//! and reported. The request is 48 kHz stereo because that is what PipeWire's graph runs at here
//! (MEASURED, `pw-metadata -n settings`: `clock.rate 48000`, `clock.allowed-rates [ 48000 ]`), so
//! on this host the server converts nothing; on a host whose graph runs at another rate the server
//! resamples, **outside this seam**, and the rate reported is still the true rate of the stream the
//! seam writes. Learning the graph's rate would need PipeWire's registry and metadata API, which
//! is a much larger binding for one number; it is the upgrade path if a host shows it matters.
//!
//! # Linking
//!
//! Hand-declared FFI under `#[link(name = "asound")]`, over the calls used -- no `-sys`
//! crate, so nothing is fetched or generated at build time. Building needs `libasound.so`
//! (Ubuntu's `libasound2-dev`); running needs `libasound.so.2`, which every desktop Linux has.
//! The two enum values and the four states this file compares against were checked against
//! alsa-lib 1.2.15's `pcm.h` by a C program printing them, and are pinned by
//! `alsa_constants_are_the_headers`.
//!
//! # What `open` does, in order
//!
//! 1. `snd_pcm_open("default", SND_PCM_STREAM_PLAYBACK, 0)` -- blocking mode, so `snd_pcm_writei`
//!    of frames that fit returns having written all of them.
//! 2. hw params: `RW_INTERLEAVED`, `FLOAT_LE` (the only sample shape the seam writes; refused by
//!    the host means [`AudioError::Alsa`] naming `snd_pcm_hw_params_set_format`), channels and rate
//!    *near* the preference, then the rate again **exactly** so that a rate that is not a whole
//!    number of hertz is refused by ALSA rather than rounded here; a period near 10 ms (what
//!    WASAPI's engine gives) and the smallest buffer of at least the frames asked for.
//! 3. `snd_pcm_hw_params` installs them, and `snd_pcm_hw_params_current` + `get_*` read back what
//!    was **granted**. That is what [`AudioOutput::format`], `buffer_frames` and `period_frames`
//!    report.
//! 4. sw params: `avail_min` one period (what `snd_pcm_wait` waits for), `start_threshold` the
//!    boundary (a write never starts the stream -- [`start`](AudioOutput::start) does, as on
//!    Windows), `stop_threshold` the buffer size (a stream that runs dry is an **xrun**, which is
//!    seen, recovered and counted rather than played through silently).
//!
//! # Start, stop, and what they keep -- the Windows backend's semantics
//!
//! WASAPI's `Stop` keeps what is queued and `Start` plays it; a written, unstarted stream does not
//! drain; a running stream that runs dry keeps running and plays what is written next. Here:
//!
//! * `stop` is `snd_pcm_pause(1)`: the queue stays, and nothing drains. Not `snd_pcm_drop`, which
//!   discards it. A host that cannot pause refuses with its own errno, named.
//! * `start` is `snd_pcm_pause(0)` from a pause and `snd_pcm_start` from prepared. A stream started
//!   with **nothing queued** is marked running and really started by the first write: ALSA refuses
//!   to start an empty playback stream (`-EPIPE`, the kernel's `snd_pcm_pre_start`), and WASAPI
//!   does not.
//! * an **xrun** (`-EPIPE`) is recovered with `snd_pcm_prepare` and counted in
//!   [`Recoveries::xruns`]; the stream is then empty, so [`writable_frames`](AudioOutput::writable_frames)
//!   answers the whole buffer -- which is what WASAPI's padding reads after an underrun, and what
//!   `omni-android`'s AAudio counts as an xrun of its own. A running stream is restarted by the
//!   next write, so the caller sees what it sees on Windows: it keeps writing, and it plays.
//! * a **suspend** (`-ESTRPIPE`, the machine slept) is `snd_pcm_resume`, retried while it answers
//!   `-EAGAIN`, then `snd_pcm_prepare` if the PCM cannot resume; counted in
//!   [`Recoveries::suspends`].
//!
//! Every other failure is [`AudioError::Alsa`] naming the call, the errno and `snd_strerror`'s
//! text.
//!
//! # Waiting
//!
//! `snd_pcm_wait` polls the PCM's descriptors until `avail >= avail_min` -- one period free -- or
//! the timeout passes. A stream that is **not** started is not waited on at all: nothing consumes
//! it, so nothing can free a period, and the seam's contract (a stream that has not been started
//! does not signal; the wait runs to its timeout) is kept by sleeping the timeout out. ALSA's own
//! poll would return at once on a prepared stream with room, which is the one place its answer and
//! WASAPI's differ.

use core::cell::Cell;
use core::ffi::{CStr, c_char, c_int, c_long, c_uint, c_ulong};
use core::ptr::{self, NonNull};
use std::time::{Duration, Instant};

use super::{AudioError, AudioResult, OutputFormat};

// ---------------------------------------------------------------------------------------------
// The FFI
// ---------------------------------------------------------------------------------------------

/// `snd_pcm_t`, opaque.
#[repr(C)]
struct SndPcm {
    _opaque: [u8; 0],
}

/// `snd_pcm_hw_params_t`, opaque (allocated by `snd_pcm_hw_params_malloc`).
#[repr(C)]
struct SndPcmHwParams {
    _opaque: [u8; 0],
}

/// `snd_pcm_sw_params_t`, opaque (allocated by `snd_pcm_sw_params_malloc`).
#[repr(C)]
struct SndPcmSwParams {
    _opaque: [u8; 0],
}

/// `snd_pcm_uframes_t`: `unsigned long`.
type Uframes = c_ulong;
/// `snd_pcm_sframes_t`: `long`.
type Sframes = c_long;

/// `SND_PCM_STREAM_PLAYBACK`.
const SND_PCM_STREAM_PLAYBACK: c_int = 0;
/// `SND_PCM_ACCESS_RW_INTERLEAVED`.
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;
/// `SND_PCM_FORMAT_FLOAT_LE`.
const SND_PCM_FORMAT_FLOAT_LE: c_int = 14;
/// `SND_PCM_FORMAT_FLOAT_BE`.
const SND_PCM_FORMAT_FLOAT_BE: c_int = 15;
/// `SND_PCM_FORMAT_FLOAT`: the host-endian float, which is what an `f32` slice is.
const SND_PCM_FORMAT_FLOAT: c_int =
    if cfg!(target_endian = "little") { SND_PCM_FORMAT_FLOAT_LE } else { SND_PCM_FORMAT_FLOAT_BE };
/// `SND_PCM_STATE_PREPARED`.
const SND_PCM_STATE_PREPARED: c_int = 2;
/// `SND_PCM_STATE_RUNNING`.
const SND_PCM_STATE_RUNNING: c_int = 3;
/// `SND_PCM_STATE_XRUN`.
const SND_PCM_STATE_XRUN: c_int = 4;
/// `SND_PCM_STATE_PAUSED`.
const SND_PCM_STATE_PAUSED: c_int = 6;
/// `SND_PCM_STATE_SUSPENDED`.
const SND_PCM_STATE_SUSPENDED: c_int = 7;

#[link(name = "asound")]
extern "C" {
    fn snd_pcm_open(pcm: *mut *mut SndPcm, name: *const c_char, stream: c_int, mode: c_int)
        -> c_int;
    fn snd_pcm_close(pcm: *mut SndPcm) -> c_int;
    fn snd_pcm_hw_params_malloc(params: *mut *mut SndPcmHwParams) -> c_int;
    fn snd_pcm_hw_params_free(params: *mut SndPcmHwParams);
    fn snd_pcm_hw_params_any(pcm: *mut SndPcm, params: *mut SndPcmHwParams) -> c_int;
    fn snd_pcm_hw_params_set_access(
        pcm: *mut SndPcm,
        params: *mut SndPcmHwParams,
        access: c_int,
    ) -> c_int;
    fn snd_pcm_hw_params_set_format(
        pcm: *mut SndPcm,
        params: *mut SndPcmHwParams,
        format: c_int,
    ) -> c_int;
    fn snd_pcm_hw_params_set_channels_near(
        pcm: *mut SndPcm,
        params: *mut SndPcmHwParams,
        val: *mut c_uint,
    ) -> c_int;
    fn snd_pcm_hw_params_set_rate_near(
        pcm: *mut SndPcm,
        params: *mut SndPcmHwParams,
        val: *mut c_uint,
        dir: *mut c_int,
    ) -> c_int;
    fn snd_pcm_hw_params_set_rate(
        pcm: *mut SndPcm,
        params: *mut SndPcmHwParams,
        val: c_uint,
        dir: c_int,
    ) -> c_int;
    fn snd_pcm_hw_params_set_period_size_near(
        pcm: *mut SndPcm,
        params: *mut SndPcmHwParams,
        val: *mut Uframes,
        dir: *mut c_int,
    ) -> c_int;
    fn snd_pcm_hw_params_set_buffer_size_min(
        pcm: *mut SndPcm,
        params: *mut SndPcmHwParams,
        val: *mut Uframes,
    ) -> c_int;
    fn snd_pcm_hw_params_set_buffer_size_first(
        pcm: *mut SndPcm,
        params: *mut SndPcmHwParams,
        val: *mut Uframes,
    ) -> c_int;
    fn snd_pcm_hw_params(pcm: *mut SndPcm, params: *mut SndPcmHwParams) -> c_int;
    fn snd_pcm_hw_params_current(pcm: *mut SndPcm, params: *mut SndPcmHwParams) -> c_int;
    fn snd_pcm_hw_params_get_access(params: *const SndPcmHwParams, access: *mut c_int) -> c_int;
    fn snd_pcm_hw_params_get_format(params: *const SndPcmHwParams, format: *mut c_int) -> c_int;
    fn snd_pcm_hw_params_get_channels(params: *const SndPcmHwParams, val: *mut c_uint) -> c_int;
    fn snd_pcm_hw_params_get_rate(
        params: *const SndPcmHwParams,
        val: *mut c_uint,
        dir: *mut c_int,
    ) -> c_int;
    fn snd_pcm_hw_params_get_period_size(
        params: *const SndPcmHwParams,
        val: *mut Uframes,
        dir: *mut c_int,
    ) -> c_int;
    fn snd_pcm_hw_params_get_buffer_size(params: *const SndPcmHwParams, val: *mut Uframes)
        -> c_int;
    fn snd_pcm_sw_params_malloc(params: *mut *mut SndPcmSwParams) -> c_int;
    fn snd_pcm_sw_params_free(params: *mut SndPcmSwParams);
    fn snd_pcm_sw_params_current(pcm: *mut SndPcm, params: *mut SndPcmSwParams) -> c_int;
    fn snd_pcm_sw_params_get_boundary(params: *const SndPcmSwParams, val: *mut Uframes) -> c_int;
    fn snd_pcm_sw_params_set_avail_min(
        pcm: *mut SndPcm,
        params: *mut SndPcmSwParams,
        val: Uframes,
    ) -> c_int;
    fn snd_pcm_sw_params_set_start_threshold(
        pcm: *mut SndPcm,
        params: *mut SndPcmSwParams,
        val: Uframes,
    ) -> c_int;
    fn snd_pcm_sw_params_set_stop_threshold(
        pcm: *mut SndPcm,
        params: *mut SndPcmSwParams,
        val: Uframes,
    ) -> c_int;
    fn snd_pcm_sw_params(pcm: *mut SndPcm, params: *mut SndPcmSwParams) -> c_int;
    fn snd_pcm_state(pcm: *mut SndPcm) -> c_int;
    fn snd_pcm_avail(pcm: *mut SndPcm) -> Sframes;
    fn snd_pcm_wait(pcm: *mut SndPcm, timeout: c_int) -> c_int;
    fn snd_pcm_writei(pcm: *mut SndPcm, buffer: *const core::ffi::c_void, size: Uframes)
        -> Sframes;
    fn snd_pcm_start(pcm: *mut SndPcm) -> c_int;
    fn snd_pcm_pause(pcm: *mut SndPcm, enable: c_int) -> c_int;
    fn snd_pcm_prepare(pcm: *mut SndPcm) -> c_int;
    fn snd_pcm_resume(pcm: *mut SndPcm) -> c_int;
    fn snd_strerror(errnum: c_int) -> *const c_char;
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// A failing ALSA return (a negative errno) as an [`AudioError::Alsa`], with `snd_strerror`'s text.
fn alsa_error(operation: &'static str, api: &'static str, code: c_int) -> AudioError {
    // SAFETY: `snd_strerror` takes any int and returns a pointer to a NUL-terminated string that
    // lives for the process (a static table, or glibc's `strerror`, copied at once below).
    let text = unsafe { snd_strerror(code) };
    let description = if text.is_null() {
        String::new()
    } else {
        // SAFETY: non-null and NUL-terminated, per the above; copied before any other call.
        unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned()
    };
    AudioError::Alsa { operation, api, errno: code.saturating_neg(), description }
}

/// An ALSA `int` return: negative is a failure, anything else is `Ok` with the value.
fn check(operation: &'static str, api: &'static str, code: c_int) -> AudioResult<c_int> {
    if code < 0 {
        return Err(alsa_error(operation, api, code));
    }
    Ok(code)
}

/// What `snd_pcm_open` failing with `code` means. `-ENOENT` (no such PCM, or no device node behind
/// it) and `-ENODEV` (the card is gone) are "no device", the state a host with no sound hardware is
/// in; everything else -- `-EBUSY` on a card another process holds, a server that refused -- is the
/// host failing.
fn open_error(code: c_int) -> AudioError {
    if code == -libc::ENOENT || code == -libc::ENODEV {
        return AudioError::NoDevice { operation: "open" };
    }
    alsa_error("open", "snd_pcm_open", code)
}

// ---------------------------------------------------------------------------------------------
// The pure parts
// ---------------------------------------------------------------------------------------------

/// The rate asked for. See this module's "Why ALSA".
pub(super) const PREFERRED_RATE: u32 = 48_000;

/// The channel count asked for. See this module's "Why ALSA".
pub(super) const PREFERRED_CHANNELS: u32 = 2;

/// The period asked for, in hundredths of a second of the granted rate: 10 ms, the period
/// WASAPI's shared-mode engine gives (MEASURED on the Windows host: 480 frames at 48 kHz).
const PERIODS_PER_SECOND: u32 = 100;

/// How long a suspended PCM is given to resume while `snd_pcm_resume` answers `-EAGAIN`.
const RESUME_WAIT: Duration = Duration::from_secs(1);

/// A timeout as `snd_pcm_wait` milliseconds: **rounded up**, so that a wait asked for in
/// microseconds does not become a zero-length poll, and capped at `c_int::MAX`, so that no finite
/// timeout becomes `-1`, which `snd_pcm_wait` reads as "wait for ever".
fn wait_millis(timeout: Duration) -> c_int {
    let millis = timeout.as_nanos().div_ceil(1_000_000);
    c_int::try_from(millis).unwrap_or(c_int::MAX)
}

/// How many times the stream has been recovered rather than failed. Counted by this backend, and
/// only ever increased. See this module's "Start, stop, and what they keep".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recoveries {
    /// Underruns (`-EPIPE`): the device ran out of queued frames and stopped; recovered with
    /// `snd_pcm_prepare`. Each one is an audible gap.
    pub xruns: u64,
    /// Suspends (`-ESTRPIPE`): the system slept under the stream; recovered with `snd_pcm_resume`,
    /// or `snd_pcm_prepare` where the PCM cannot resume.
    pub suspends: u64,
}

/// The two calls recovery makes, so that [`recover`] can be run against a recording double as well
/// as a PCM. Each returns what the ALSA call of the same name returns.
trait Recover {
    fn prepare(&self) -> c_int;
    fn resume(&self) -> c_int;
}

/// Recover from `code`, which `api` returned inside `operation`, or return it as the failure.
///
/// `-EPIPE` is an xrun: `snd_pcm_prepare`, and one more [`Recoveries::xruns`]. `-ESTRPIPE` is a
/// suspend: `snd_pcm_resume` while it answers `-EAGAIN` (for at most [`RESUME_WAIT`]), then
/// `snd_pcm_prepare` if it could not resume, and one more [`Recoveries::suspends`]. The count is
/// charged only once the recovery has succeeded, so it counts streams brought back, and a failed
/// recovery is an error naming the recovery call. Any other `code` is not recoverable and is
/// returned as the failure of `api`.
fn recover(
    pcm: &impl Recover,
    counts: &Cell<Recoveries>,
    operation: &'static str,
    api: &'static str,
    code: c_int,
) -> AudioResult<()> {
    let mut now = counts.get();
    if code == -libc::EPIPE {
        check(operation, "snd_pcm_prepare", pcm.prepare())?;
        now.xruns += 1;
    } else if code == -libc::ESTRPIPE {
        let deadline = Instant::now() + RESUME_WAIT;
        let resumed = loop {
            let resumed = pcm.resume();
            if resumed != -libc::EAGAIN || Instant::now() >= deadline {
                break resumed;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        if resumed == -libc::EAGAIN {
            return Err(alsa_error(operation, "snd_pcm_resume", resumed));
        }
        if resumed < 0 {
            // The PCM cannot resume (a plugin without resume answers -ENOSYS): ALSA's documented
            // fallback is to prepare it, which loses what was queued -- as the sleep already did.
            check(operation, "snd_pcm_prepare", pcm.prepare())?;
        }
        now.suspends += 1;
    } else {
        return Err(alsa_error(operation, api, code));
    }
    counts.set(now);
    Ok(())
}

/// What a stream is opened with. [`AudioOutput::open`] uses [`Request::default_device`]; the
/// unit tests ask for other shapes, to see that what is reported is what was **granted**.
#[derive(Debug, Clone, Copy)]
pub(super) struct Request<'a> {
    pub(super) device: &'a CStr,
    pub(super) rate: u32,
    pub(super) channels: u32,
    /// The least buffer wanted, in frames; 0 is the smallest the host gives.
    pub(super) buffer_frames: u32,
    /// The period wanted, in frames; 0 is 10 ms at the granted rate.
    pub(super) period_frames: u32,
}

impl Request<'_> {
    fn default_device(buffer_frames: u32) -> Request<'static> {
        Request {
            device: c"default",
            rate: PREFERRED_RATE,
            channels: PREFERRED_CHANNELS,
            buffer_frames,
            period_frames: 0,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Owned ALSA objects
// ---------------------------------------------------------------------------------------------

/// An open PCM, closed exactly once when dropped.
struct Pcm(NonNull<SndPcm>);

impl Pcm {
    fn raw(&self) -> *mut SndPcm {
        self.0.as_ptr()
    }

    fn state(&self) -> c_int {
        // SAFETY: a live PCM.
        unsafe { snd_pcm_state(self.raw()) }
    }
}

impl Recover for Pcm {
    fn prepare(&self) -> c_int {
        // SAFETY: a live PCM.
        unsafe { snd_pcm_prepare(self.raw()) }
    }
    fn resume(&self) -> c_int {
        // SAFETY: a live PCM.
        unsafe { snd_pcm_resume(self.raw()) }
    }
}

impl Drop for Pcm {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful `snd_pcm_open` and is closed once, here.
        // `snd_pcm_close` drops a running stream first. The result is ignored: `Drop` has nobody
        // to report it to.
        unsafe { snd_pcm_close(self.raw()) };
    }
}

/// A `snd_pcm_hw_params_malloc`'d parameter block, freed when dropped.
struct HwParams(NonNull<SndPcmHwParams>);

impl HwParams {
    fn new() -> AudioResult<Self> {
        let mut raw = ptr::null_mut();
        // SAFETY: an out-pointer live for the call.
        check("open", "snd_pcm_hw_params_malloc", unsafe { snd_pcm_hw_params_malloc(&mut raw) })?;
        Ok(HwParams(NonNull::new(raw).expect("snd_pcm_hw_params_malloc succeeded with null")))
    }
    fn raw(&self) -> *mut SndPcmHwParams {
        self.0.as_ptr()
    }
}

impl Drop for HwParams {
    fn drop(&mut self) {
        // SAFETY: allocated by `snd_pcm_hw_params_malloc`, freed once.
        unsafe { snd_pcm_hw_params_free(self.raw()) };
    }
}

/// A `snd_pcm_sw_params_malloc`'d parameter block, freed when dropped.
struct SwParams(NonNull<SndPcmSwParams>);

impl SwParams {
    fn new() -> AudioResult<Self> {
        let mut raw = ptr::null_mut();
        // SAFETY: an out-pointer live for the call.
        check("open", "snd_pcm_sw_params_malloc", unsafe { snd_pcm_sw_params_malloc(&mut raw) })?;
        Ok(SwParams(NonNull::new(raw).expect("snd_pcm_sw_params_malloc succeeded with null")))
    }
    fn raw(&self) -> *mut SndPcmSwParams {
        self.0.as_ptr()
    }
}

impl Drop for SwParams {
    fn drop(&mut self) {
        // SAFETY: allocated by `snd_pcm_sw_params_malloc`, freed once.
        unsafe { snd_pcm_sw_params_free(self.raw()) };
    }
}

/// A frame count ALSA reported, as the seam's `u32`. Saturates: a buffer of four billion frames is
/// not one any host grants, and a wrapped count would be a small one (VERIFICATION entry 3).
fn frames_u32(frames: Uframes) -> u32 {
    u32::try_from(frames).unwrap_or(u32::MAX)
}

// ---------------------------------------------------------------------------------------------
// The stream
// ---------------------------------------------------------------------------------------------

/// An ALSA playback PCM. See this module's header.
pub(super) struct AudioOutput {
    pcm: Pcm,
    format: OutputFormat,
    buffer_frames: u32,
    period_frames: u32,
    /// Whether [`AudioOutput::start`] has succeeded without a [`AudioOutput::stop`] since -- the
    /// caller's view, which is what WASAPI's `running` is. The PCM itself can be `PREPARED` while
    /// this is true: started with nothing queued, or recovered from an xrun, and waiting for the
    /// next write to start it.
    running: bool,
    counts: Cell<Recoveries>,
}

// SAFETY: an ALSA PCM handle is not tied to the thread that opened it; alsa-lib's rule is only that
// one handle is not used by two threads at once. `AudioOutput` in `mod.rs` is `!Sync`, and every
// call here goes through it, so the handle is used by one thread at a time. The parameter blocks
// never leave `open`.
unsafe impl Send for AudioOutput {}

impl AudioOutput {
    pub(super) fn open(buffer_frames: u32) -> AudioResult<Self> {
        Self::open_with(Request::default_device(buffer_frames))
    }

    pub(super) fn open_with(request: Request<'_>) -> AudioResult<Self> {
        let mut raw = ptr::null_mut();
        // SAFETY: an out-pointer and a NUL-terminated name, both live for the call.
        let code = unsafe {
            snd_pcm_open(&mut raw, request.device.as_ptr(), SND_PCM_STREAM_PLAYBACK, 0)
        };
        if code < 0 {
            return Err(open_error(code));
        }
        let pcm = Pcm(NonNull::new(raw).expect("snd_pcm_open succeeded with a null handle"));
        let p = pcm.raw();

        let hw = HwParams::new()?;
        let h = hw.raw();
        // SAFETY (every call in this block): `p` is the live PCM above and `h` a live parameter
        // block owned by `hw`; every out-pointer is a local live for its call.
        unsafe {
            check("open", "snd_pcm_hw_params_any", snd_pcm_hw_params_any(p, h))?;
            check(
                "open",
                "snd_pcm_hw_params_set_access(SND_PCM_ACCESS_RW_INTERLEAVED)",
                snd_pcm_hw_params_set_access(p, h, SND_PCM_ACCESS_RW_INTERLEAVED),
            )?;
            check(
                "open",
                "snd_pcm_hw_params_set_format(SND_PCM_FORMAT_FLOAT)",
                snd_pcm_hw_params_set_format(p, h, SND_PCM_FORMAT_FLOAT),
            )?;
            let mut channels: c_uint = request.channels;
            check(
                "open",
                "snd_pcm_hw_params_set_channels_near",
                snd_pcm_hw_params_set_channels_near(p, h, &mut channels),
            )?;
            let (mut rate, mut dir): (c_uint, c_int) = (request.rate, 0);
            check(
                "open",
                "snd_pcm_hw_params_set_rate_near",
                snd_pcm_hw_params_set_rate_near(p, h, &mut rate, &mut dir),
            )?;
            // Exactly, with `dir` 0: a rate that is not a whole number of hertz (`dir` non-zero
            // above) cannot be said in `OutputFormat`, and ALSA refuses it here with its own errno
            // rather than this backend rounding it.
            check("open", "snd_pcm_hw_params_set_rate", snd_pcm_hw_params_set_rate(p, h, rate, 0))?;
            let mut period: Uframes = if request.period_frames == 0 {
                Uframes::from(rate.div_ceil(PERIODS_PER_SECOND))
            } else {
                Uframes::from(request.period_frames)
            };
            let mut dir: c_int = 0;
            check(
                "open",
                "snd_pcm_hw_params_set_period_size_near",
                snd_pcm_hw_params_set_period_size_near(p, h, &mut period, &mut dir),
            )?;
            let mut least: Uframes = Uframes::from(request.buffer_frames);
            if least > 0 {
                check(
                    "open",
                    "snd_pcm_hw_params_set_buffer_size_min",
                    snd_pcm_hw_params_set_buffer_size_min(p, h, &mut least),
                )?;
            }
            let mut buffer: Uframes = 0;
            check(
                "open",
                "snd_pcm_hw_params_set_buffer_size_first",
                snd_pcm_hw_params_set_buffer_size_first(p, h, &mut buffer),
            )?;
            check("open", "snd_pcm_hw_params", snd_pcm_hw_params(p, h))?;
        }

        // What was granted, read back from the installed configuration rather than from the
        // values the setters wrote back: this is what the PCM runs at.
        let hw = HwParams::new()?;
        let h = hw.raw();
        let (mut access, mut sample_format, mut channels, mut rate, mut rate_dir) = (0, 0, 0, 0, 0);
        let (mut period, mut period_dir, mut buffer): (Uframes, c_int, Uframes) = (0, 0, 0);
        // SAFETY: as above; `snd_pcm_hw_params_current` fills `h` from the installed setup.
        unsafe {
            check("open", "snd_pcm_hw_params_current", snd_pcm_hw_params_current(p, h))?;
            check("open", "snd_pcm_hw_params_get_access", snd_pcm_hw_params_get_access(h, &mut access))?;
            check(
                "open",
                "snd_pcm_hw_params_get_format",
                snd_pcm_hw_params_get_format(h, &mut sample_format),
            )?;
            check(
                "open",
                "snd_pcm_hw_params_get_channels",
                snd_pcm_hw_params_get_channels(h, &mut channels),
            )?;
            check(
                "open",
                "snd_pcm_hw_params_get_rate",
                snd_pcm_hw_params_get_rate(h, &mut rate, &mut rate_dir),
            )?;
            check(
                "open",
                "snd_pcm_hw_params_get_period_size",
                snd_pcm_hw_params_get_period_size(h, &mut period, &mut period_dir),
            )?;
            check(
                "open",
                "snd_pcm_hw_params_get_buffer_size",
                snd_pcm_hw_params_get_buffer_size(h, &mut buffer),
            )?;
        }
        // Each of these was fixed by a setter above on the same configuration that was installed,
        // so the installed value is that one: the access and format by `set_access`/`set_format`,
        // the rate by `set_rate(.., 0)`, which admits no fraction. Asserted, not branched on
        // (VERIFICATION entry 12).
        debug_assert_eq!(access, SND_PCM_ACCESS_RW_INTERLEAVED);
        debug_assert_eq!(sample_format, SND_PCM_FORMAT_FLOAT);
        debug_assert_eq!(rate_dir, 0, "a fractional rate was installed");
        let format = OutputFormat {
            sample_rate: rate,
            channels: u16::try_from(channels).unwrap_or(u16::MAX),
        };
        let buffer_frames = frames_u32(buffer);
        let period_frames = frames_u32(period);

        let sw = SwParams::new()?;
        let s = sw.raw();
        let mut boundary: Uframes = 0;
        // SAFETY: the live PCM and a live parameter block owned by `sw`; out-pointers are locals.
        unsafe {
            check("open", "snd_pcm_sw_params_current", snd_pcm_sw_params_current(p, s))?;
            check(
                "open",
                "snd_pcm_sw_params_get_boundary",
                snd_pcm_sw_params_get_boundary(s, &mut boundary),
            )?;
            check(
                "open",
                "snd_pcm_sw_params_set_avail_min",
                snd_pcm_sw_params_set_avail_min(p, s, period),
            )?;
            check(
                "open",
                "snd_pcm_sw_params_set_start_threshold",
                snd_pcm_sw_params_set_start_threshold(p, s, boundary),
            )?;
            check(
                "open",
                "snd_pcm_sw_params_set_stop_threshold",
                snd_pcm_sw_params_set_stop_threshold(p, s, buffer),
            )?;
            check("open", "snd_pcm_sw_params", snd_pcm_sw_params(p, s))?;
        }

        Ok(AudioOutput {
            pcm,
            format,
            buffer_frames,
            period_frames,
            running: false,
            counts: Cell::new(Recoveries::default()),
        })
    }

    pub(super) fn format(&self) -> OutputFormat {
        self.format
    }

    pub(super) fn buffer_frames(&self) -> u32 {
        self.buffer_frames
    }

    pub(super) fn period_frames(&self) -> u32 {
        self.period_frames
    }

    /// The recoveries so far.
    pub(super) fn recoveries(&self) -> Recoveries {
        self.counts.get()
    }

    /// Recover from `code`; see [`recover`].
    fn recover(&self, operation: &'static str, api: &'static str, code: c_int) -> AudioResult<()> {
        recover(&self.pcm, &self.counts, operation, api, code)
    }

    /// `snd_pcm_avail`: the free space, synchronised with the device's position first (where
    /// `snd_pcm_avail_update` answers from the last position the driver reported). An xrun or a
    /// suspend is recovered and asked again; a second failure is the answer.
    pub(super) fn writable_frames(&self, operation: &'static str) -> AudioResult<u32> {
        // SAFETY: a live PCM.
        let mut avail = unsafe { snd_pcm_avail(self.pcm.raw()) };
        if avail < 0 {
            self.recover(operation, "snd_pcm_avail", c_int::try_from(avail).unwrap_or(c_int::MIN))?;
            // SAFETY: as above.
            avail = unsafe { snd_pcm_avail(self.pcm.raw()) };
            if avail < 0 {
                let code = c_int::try_from(avail).unwrap_or(c_int::MIN);
                return Err(alsa_error(operation, "snd_pcm_avail", code));
            }
        }
        // Never more than the buffer: with `stop_threshold` at the buffer size a running PCM that
        // reaches an empty buffer is an xrun, so this can only saturate in the instant between the
        // device running dry and the driver saying so -- when "all of it is free" is true.
        Ok(frames_u32(Uframes::try_from(avail).unwrap_or(0)).min(self.buffer_frames))
    }

    /// `snd_pcm_wait` on a started stream; the timeout slept out on one that is not. Then
    /// [`AudioOutput::writable_frames`]. See this module's "Waiting".
    pub(super) fn wait_writable(&self, timeout: Duration) -> AudioResult<u32> {
        if !self.running {
            std::thread::sleep(timeout);
            return self.writable_frames("wait_writable");
        }
        // SAFETY: a live PCM, and a timeout that is never negative (`wait_millis`).
        let waited = unsafe { snd_pcm_wait(self.pcm.raw(), wait_millis(timeout)) };
        if waited < 0 {
            self.recover("wait_writable", "snd_pcm_wait", waited)?;
        }
        // 1 (a period is free) and 0 (the timeout passed) both come here: either way the answer is
        // what is writable now.
        self.writable_frames("wait_writable")
    }

    /// `snd_pcm_writei` until every frame is in, recovering an xrun or a suspend on the way, then
    /// start the PCM if the caller had started it and it is waiting for data. The caller has checked
    /// that `frames` fits.
    pub(super) fn write(&mut self, samples: &[f32], frames: u32) -> AudioResult<()> {
        let channels = usize::from(self.format.channels);
        assert_eq!(
            samples.len(),
            frames as usize * channels,
            "write: {} samples is not {frames} frames of {channels} channels",
            samples.len()
        );
        let mut done = 0usize;
        let frames = frames as usize;
        while done < frames {
            let rest = &samples[done * channels..];
            // SAFETY: a live PCM; `rest` is `(frames - done) * channels` initialised `f32`s, which
            // is `frames - done` frames of the installed format (host-endian float, interleaved,
            // `channels` per frame), and ALSA only reads it.
            let wrote = unsafe {
                snd_pcm_writei(self.pcm.raw(), rest.as_ptr().cast(), (frames - done) as Uframes)
            };
            if wrote < 0 {
                // The device ran dry (or slept) partway: what was already written has played, and
                // the rest goes into the recovered, empty buffer.
                self.recover("write", "snd_pcm_writei", c_int::try_from(wrote).unwrap_or(c_int::MIN))?;
                continue;
            }
            // A blocking `snd_pcm_writei` of a non-zero count returns it whole or reports an error
            // (alsa-lib `snd_pcm_write_areas` returns `xfer > 0 ? xfer : err`), so progress is
            // at least one frame here and the loop ends.
            done += usize::try_from(wrote).unwrap_or(0);
        }
        if self.running && self.pcm.state() == SND_PCM_STATE_PREPARED {
            // SAFETY: a live PCM, prepared, with frames queued.
            check("write", "snd_pcm_start", unsafe { snd_pcm_start(self.pcm.raw()) })?;
        }
        Ok(())
    }

    pub(super) fn start(&mut self) -> AudioResult<()> {
        if self.running {
            return Ok(());
        }
        let state = self.pcm.state();
        if state == SND_PCM_STATE_XRUN || state == SND_PCM_STATE_SUSPENDED {
            let code = if state == SND_PCM_STATE_XRUN { -libc::EPIPE } else { -libc::ESTRPIPE };
            self.recover("start", "snd_pcm_state", code)?;
        }
        match self.pcm.state() {
            SND_PCM_STATE_PAUSED => {
                // SAFETY: a live, paused PCM.
                check("start", "snd_pcm_pause(0)", unsafe { snd_pcm_pause(self.pcm.raw(), 0) })?;
            }
            SND_PCM_STATE_PREPARED if self.writable_frames("start")? < self.buffer_frames => {
                // SAFETY: a live, prepared PCM with frames queued.
                check("start", "snd_pcm_start", unsafe { snd_pcm_start(self.pcm.raw()) })?;
            }
            // Prepared with nothing queued: ALSA will not start an empty playback stream, so the
            // first write starts it (see `write`). Any other state is ALSA's to refuse on the
            // next call, by name.
            _ => {}
        }
        self.running = true;
        Ok(())
    }

    pub(super) fn stop(&mut self) -> AudioResult<()> {
        if !self.running {
            return Ok(());
        }
        if self.pcm.state() == SND_PCM_STATE_RUNNING {
            // SAFETY: a live, running PCM.
            check("stop", "snd_pcm_pause(1)", unsafe { snd_pcm_pause(self.pcm.raw(), 1) })?;
        }
        // Prepared (never started, or recovered and not yet restarted) or in an xrun: nothing is
        // draining, so there is nothing to pause, and what is queued stays queued.
        self.running = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values this file declares by hand, as alsa-lib 1.2.15's `pcm.h` numbers them (read by a
    /// C program printing each one). The enum order in the header is what fixes them.
    #[test]
    fn alsa_constants_are_the_headers() {
        assert_eq!(SND_PCM_STREAM_PLAYBACK, 0);
        assert_eq!(SND_PCM_ACCESS_RW_INTERLEAVED, 3);
        assert_eq!(SND_PCM_FORMAT_FLOAT_LE, 14);
        assert_eq!(SND_PCM_FORMAT_FLOAT_BE, 15);
        assert_eq!(
            (SND_PCM_STATE_PREPARED, SND_PCM_STATE_RUNNING, SND_PCM_STATE_XRUN),
            (2, 3, 4)
        );
        assert_eq!((SND_PCM_STATE_PAUSED, SND_PCM_STATE_SUSPENDED), (6, 7));
        assert_eq!(size_of::<Uframes>(), size_of::<usize>(), "unsigned long is pointer-sized on LP64");
    }

    #[test]
    fn timeouts_round_up_to_milliseconds_and_never_become_infinite() {
        assert_eq!(wait_millis(Duration::ZERO), 0);
        assert_eq!(wait_millis(Duration::from_nanos(1)), 1, "a microsecond wait is not a poll");
        assert_eq!(wait_millis(Duration::from_millis(1)), 1);
        assert_eq!(wait_millis(Duration::from_micros(1_500)), 2);
        assert_eq!(wait_millis(Duration::from_secs(u64::MAX)), c_int::MAX, "not -1");
        assert_eq!(wait_millis(Duration::MAX), c_int::MAX);
    }

    #[test]
    fn an_alsa_error_names_the_call_the_errno_and_the_text() {
        let error = alsa_error("write", "snd_pcm_writei", -libc::EBADFD);
        let AudioError::Alsa { operation, api, errno, ref description } = error else {
            panic!("{error:?}");
        };
        assert_eq!((operation, api, errno), ("write", "snd_pcm_writei", libc::EBADFD));
        assert_eq!(description, "File descriptor in bad state", "snd_strerror's text");
        let text = error.to_string();
        assert!(text.contains("snd_pcm_writei") && text.contains("77"), "{text}");
        assert!(!error.is_unsupported());
    }

    #[test]
    fn open_failures_that_mean_no_device_are_no_device() {
        assert_eq!(open_error(-libc::ENOENT), AudioError::NoDevice { operation: "open" });
        assert_eq!(open_error(-libc::ENODEV), AudioError::NoDevice { operation: "open" });
        assert!(
            matches!(open_error(-libc::EBUSY), AudioError::Alsa { api: "snd_pcm_open", errno, .. } if errno == libc::EBUSY),
            "a busy card is the host failing, not an absent one"
        );
    }

    /// A PCM double that answers `resume` from a script and records what was called.
    struct Scripted {
        resumes: Cell<Vec<c_int>>,
        prepare: c_int,
        calls: Cell<Vec<&'static str>>,
    }

    impl Scripted {
        fn new(resumes: Vec<c_int>, prepare: c_int) -> Self {
            Scripted { resumes: Cell::new(resumes), prepare, calls: Cell::new(Vec::new()) }
        }
        fn log(&self, call: &'static str) {
            let mut calls = self.calls.take();
            calls.push(call);
            self.calls.set(calls);
        }
        fn calls(&self) -> Vec<&'static str> {
            let calls = self.calls.take();
            self.calls.set(calls.clone());
            calls
        }
    }

    impl Recover for Scripted {
        fn prepare(&self) -> c_int {
            self.log("prepare");
            self.prepare
        }
        fn resume(&self) -> c_int {
            self.log("resume");
            let mut script = self.resumes.take();
            let next = if script.is_empty() { 0 } else { script.remove(0) };
            self.resumes.set(script);
            next
        }
    }

    #[test]
    fn an_xrun_is_prepared_and_counted() {
        let pcm = Scripted::new(vec![], 0);
        let counts = Cell::new(Recoveries::default());
        recover(&pcm, &counts, "write", "snd_pcm_writei", -libc::EPIPE).unwrap();
        recover(&pcm, &counts, "write", "snd_pcm_writei", -libc::EPIPE).unwrap();
        assert_eq!(pcm.calls(), ["prepare", "prepare"]);
        assert_eq!(counts.get(), Recoveries { xruns: 2, suspends: 0 });
    }

    #[test]
    fn an_xrun_whose_prepare_fails_is_an_error_naming_prepare_and_is_not_counted() {
        let pcm = Scripted::new(vec![], -libc::ENODEV);
        let counts = Cell::new(Recoveries::default());
        let error = recover(&pcm, &counts, "wait_writable", "snd_pcm_wait", -libc::EPIPE).unwrap_err();
        assert!(
            matches!(error, AudioError::Alsa { operation: "wait_writable", api: "snd_pcm_prepare", errno, .. } if errno == libc::ENODEV),
            "{error:?}"
        );
        assert_eq!(counts.get(), Recoveries::default());
    }

    #[test]
    fn a_suspend_resumes_through_eagain_and_is_counted() {
        let pcm = Scripted::new(vec![-libc::EAGAIN, -libc::EAGAIN, 0], 0);
        let counts = Cell::new(Recoveries::default());
        recover(&pcm, &counts, "write", "snd_pcm_writei", -libc::ESTRPIPE).unwrap();
        assert_eq!(pcm.calls(), ["resume", "resume", "resume"], "no prepare after a resume");
        assert_eq!(counts.get(), Recoveries { xruns: 0, suspends: 1 });
    }

    #[test]
    fn a_suspend_that_cannot_resume_is_prepared() {
        let pcm = Scripted::new(vec![-libc::ENOSYS], 0);
        let counts = Cell::new(Recoveries::default());
        recover(&pcm, &counts, "write", "snd_pcm_writei", -libc::ESTRPIPE).unwrap();
        assert_eq!(pcm.calls(), ["resume", "prepare"]);
        assert_eq!(counts.get(), Recoveries { xruns: 0, suspends: 1 });
    }

    #[test]
    fn any_other_failure_is_returned_naming_the_call_and_recovers_nothing() {
        let pcm = Scripted::new(vec![], 0);
        let counts = Cell::new(Recoveries::default());
        let error = recover(&pcm, &counts, "write", "snd_pcm_writei", -libc::EBADFD).unwrap_err();
        assert!(
            matches!(error, AudioError::Alsa { api: "snd_pcm_writei", errno, .. } if errno == libc::EBADFD),
            "{error:?}"
        );
        assert!(pcm.calls().is_empty(), "{:?}", pcm.calls());
        assert_eq!(counts.get(), Recoveries::default());
    }

    // ---- live: these open the host's real PCM, behind the same gate as tests/audio_live.rs ----

    const GATE: &str = "OMNI_AUDIO_LIVE_TESTS";

    fn require_gate() {
        assert!(
            std::env::var(GATE).is_ok_and(|v| v == "1"),
            "run with --ignored but {GATE} is not 1; this test opens the host's real playback \
             device (silence only) and will not pretend to pass without one"
        );
    }

    /// What is reported is what ALSA **granted**, not what was asked: a request no host grants as
    /// asked -- a million frames a second, two hundred channels -- comes back as the host's own
    /// nearest values, and those are what `format` says.
    #[test]
    #[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
    fn the_format_reported_is_the_one_granted_not_the_one_asked_for() {
        require_gate();
        let asked = Request {
            device: c"default",
            rate: 1_000_000,
            channels: 200,
            buffer_frames: 0,
            period_frames: 0,
        };
        let output = AudioOutput::open_with(asked).unwrap();
        let granted = output.format();
        println!(
            "asked {} Hz x{}: granted {} Hz x{}, buffer {}, period {}",
            asked.rate,
            asked.channels,
            granted.sample_rate,
            granted.channels,
            output.buffer_frames(),
            output.period_frames()
        );
        assert_ne!(granted.sample_rate, asked.rate, "a million frames a second was granted?");
        assert_ne!(u32::from(granted.channels), asked.channels, "two hundred channels were granted?");
        assert!(granted.sample_rate > 0 && granted.channels > 0, "{granted:?}");
        // The seam's own contract on top: a fresh stream is empty, all of it writable.
        assert_eq!(output.writable_frames("writable_frames"), Ok(output.buffer_frames()));
    }

    /// `snd_pcm_wait`'s timeout is honoured on a **running** stream: with a period of 200 ms and a
    /// full buffer, a period cannot free for about 200 ms, so a 20 ms wait must come back long
    /// before one does. A wait that ignored its timeout would come back at the period. Five rounds,
    /// each refilled first.
    #[test]
    #[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
    fn a_running_wait_returns_at_its_timeout_when_no_period_has_freed() {
        require_gate();
        let request = Request {
            device: c"default",
            rate: PREFERRED_RATE,
            channels: PREFERRED_CHANNELS,
            buffer_frames: PREFERRED_RATE, // a second
            period_frames: PREFERRED_RATE / 5, // 200 ms
        };
        let mut output = AudioOutput::open_with(request).unwrap();
        let (period, rate) = (output.period_frames(), output.format().sample_rate);
        let period_time = Duration::from_secs_f64(f64::from(period) / f64::from(rate));
        assert!(period_time >= Duration::from_millis(100), "granted period {period} at {rate} Hz");
        let channels = usize::from(output.format().channels);
        let fill = |output: &mut AudioOutput| {
            let free = output.writable_frames("write").unwrap();
            output.write(&vec![0.0; free as usize * channels], free).unwrap();
        };
        fill(&mut output);
        output.start().unwrap();
        let timeout = Duration::from_millis(20);
        let mut took = Vec::new();
        for _ in 0..5 {
            fill(&mut output);
            let asked = Instant::now();
            let free = output.wait_writable(timeout).unwrap();
            let waited = asked.elapsed();
            took.push(waited);
            assert!(
                waited < period_time / 2,
                "a {timeout:?} wait on a full running stream took {waited:?} (period {period_time:?}, \
                 {free} frames free): it waited for the period, not the timeout"
            );
        }
        println!("period {period} frames ({period_time:?}); five {timeout:?} waits took {took:?}");
        assert_eq!(output.recoveries(), Recoveries::default(), "a full buffer ran dry");
    }

    /// The card named by `OMNI_AUDIO_HW_CARD`, opened **without the sound server** -- the path a
    /// host with no PipeWire takes. `hw:` is the card as it is: a card that does not do float
    /// (every HDA codec) refuses by name at `snd_pcm_hw_params_set_format`, the seam's "no
    /// conversion here". `plughw:` is alsa-lib's own conversion layer over it, which is what such
    /// a host's `"default"` is, and it must open at the preference and be consumed at its rate.
    ///
    /// Separately gated, because it needs the card free: a running sound server holds it
    /// (`EBUSY`) until it suspends an idle sink, so the open is retried for up to ten seconds
    /// while it is busy, and fails after that.
    #[test]
    #[ignore = "needs a card and no server using it: OMNI_AUDIO_LIVE_TESTS=1 OMNI_AUDIO_HW_CARD=<name> cargo test -- --ignored"]
    fn hw_card_a_card_opened_without_the_server_refuses_float_by_name_or_plays_it() {
        require_gate();
        let card = std::env::var("OMNI_AUDIO_HW_CARD").unwrap_or_else(|_| {
            panic!("run with --ignored but OMNI_AUDIO_HW_CARD is not set to a card name (aplay -L)")
        });
        let open_free = |name: &str| {
            let device = std::ffi::CString::new(name).unwrap();
            let request = Request { device: &device, ..Request::default_device(PREFERRED_RATE / 5) };
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match AudioOutput::open_with(request) {
                    Err(AudioError::Alsa { api: "snd_pcm_open", errno, .. })
                        if errno == libc::EBUSY && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    other => break other,
                }
            }
        };

        let raw = format!("hw:CARD={card},DEV=0");
        match open_free(&raw) {
            Err(AudioError::Alsa { api, errno, .. }) => {
                assert_eq!(
                    (api, errno),
                    ("snd_pcm_hw_params_set_format(SND_PCM_FORMAT_FLOAT)", libc::EINVAL),
                    "{raw}"
                );
                println!("{raw}: float refused at {api}, errno {errno}");
            }
            Ok(output) => println!("{raw}: the card takes float itself: {output:?}", output = output.format()),
            Err(other) => panic!("{raw}: {other}"),
        }

        let plug = format!("plughw:CARD={card},DEV=0");
        let mut output = open_free(&plug).unwrap_or_else(|e| panic!("{plug}: {e}"));
        let format = output.format();
        assert_eq!((format.sample_rate, format.channels), (PREFERRED_RATE, 2), "{plug}");
        let channels = usize::from(format.channels);
        let buffer = output.buffer_frames();
        let mut written = 0u64;
        let mut top_up = |output: &mut AudioOutput, written: &mut u64| {
            let free = output.writable_frames("write").unwrap();
            output.write(&vec![0.0; free as usize * channels], free).unwrap();
            *written += u64::from(free);
        };
        top_up(&mut output, &mut written);
        output.start().unwrap();
        let started = Instant::now();
        let consumed = |output: &AudioOutput, written: u64| {
            written - u64::from(buffer - output.writable_frames("writable_frames").unwrap())
        };
        let mut feed = |output: &mut AudioOutput, written: &mut u64, until: Duration| {
            while started.elapsed() < until {
                output.wait_writable(Duration::from_millis(100)).unwrap();
                top_up(output, written);
            }
        };
        feed(&mut output, &mut written, Duration::from_millis(200));
        let (from, from_at) = (consumed(&output, written), started.elapsed());
        feed(&mut output, &mut written, Duration::from_millis(1_400));
        let (to, to_at) = (consumed(&output, written), started.elapsed());
        let per_second = (to - from) as f64 / (to_at - from_at).as_secs_f64();
        println!(
            "{plug}: {format:?}, buffer {buffer}, period {}; {per_second:.0} frames/s over {:?}; \
             recoveries {:?}",
            output.period_frames(),
            to_at - from_at,
            output.recoveries()
        );
        assert!((0.98..=1.02).contains(&(per_second / f64::from(PREFERRED_RATE))), "{per_second}");
        assert_eq!(output.recoveries(), Recoveries::default());
    }
}
