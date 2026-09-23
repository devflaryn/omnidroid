//! `libaaudio.so`: the NDK's audio stream API, as the engine's FMOD reaches it -- and the host's
//! audio output behind it.
//!
//! # Why this layer supplies a library the guest never links against
//!
//! `libroblox.so` has **no AAudio import**. FMOD's Android output reaches it the way the Vulkan
//! loader is reached, by name: its driver-info and init functions start with
//! `dlopen("libaaudio.so")` (`0x4fbf3dc`) and then `dlsym` every entry point they use (`0x4fbf3f8`
//! onwards, into the table at `0x6d0ef20`). With no library behind that name `dlopen` answers NULL,
//! `System::init` fails with `FMOD_ERR_OUTPUT_INIT` (51) -- MEASURED in every gate run so far,
//! `[FLog::Audio] FMOD initialization failed with error code 51!` -- and Roblox falls back to
//! `FMOD_OUTPUTTYPE_NOSOUND` (`jni::classes`, the `org/fmod/FMOD` declaration, has that decode).
//! That NULL was true: this runtime had no audio output. This module is the output, so the NULL is
//! no longer true once an embedding binds it -- and **only then**: `bionic::dl` issues a handle for
//! `libaaudio.so` only when [`ENTRY_POINT`] is bound, exactly as it does for `libvulkan.so`.
//!
//! # What FMOD asks of it -- DECODED, and all this module exports
//!
//! [`EXPORTS`] is FMOD's `dlsym` list, in its order. Every one is required (a NULL fails init with
//! 51) except `AAudioStream_waitForStateChange` (fetched only behind a flag), `setFormat` (a NULL is
//! replaced by a no-op), `setUsage` (its result is not checked) and `setInputPreset` (behind a
//! flag). The output stream is opened at `0x4fbf74c`:
//!
//! ```text
//! b = AAudio_createStreamBuilder()
//! setPerformanceMode(b, LOW_LATENCY) ; setUsage(b, GAME) ; setDirection(b, OUTPUT)
//! setDataCallback(b, 0x4fbfbd8, fmod) ; setErrorCallback(b, 0x4fbfc50, fmod)      -- no format
//! probe = openStream(b) ; size = getBufferSizeInFrames ; rate = getSampleRate
//! burst = getFramesPerBurst ; capacity = getBufferCapacityInFrames ; close(probe)
//! if capacity < 2 * size: setBufferCapacityInFrames(b, larger)
//! s = openStream(b) ; getFormat (I16 -> PCM16, FLOAT -> PCMFLOAT) ; getChannelCount
//! getBufferCapacityInFrames ; setBufferSizeInFrames(s, size) ; delete(b)
//! ```
//!
//! So the sample rate and channel count are **the stream's to choose**, and the format too: FMOD
//! reads all three back. This module chooses what is true of the host -- the output device's own
//! mix rate and channel count, and `PCM_FLOAT`, which is that mix format's sample type.
//!
//! # The data callback runs on a guest thread this module starts through the guest's own
//! `pthread_create`
//!
//! AAudio calls the data callback from a thread of its own. Here that thread must be a **guest**
//! thread: the callback is guest code, it uses TLS and `errno`, and it takes guest locks (FMOD's
//! callback, `0x4fbfbd8`, copies from its ring buffer and posts its mixer's semaphore). So
//! `requestStart` calls the `pthread_create` thunk -- through [`ReentrantCall::call_guest`], exactly
//! as guest code would -- with [`THREAD_ENTRY`]'s thunk as the start routine, and detaches it. That
//! thread therefore has everything `pthread_create` gives any guest thread (a stack with a guard, a
//! TLS block, a `pthread_t`, the instances the thread host carries, the stop switch), and nothing
//! here re-implements any of it. Its start routine is a handler: a loop that waits for the host
//! device to want data, calls the guest's callback for one burst at a time into a guest buffer,
//! converts and writes. It returns when the stream is paused, stopped or closed, when the callback
//! answers `STOP`, when the host device fails (the stream is then `DISCONNECTED` and the guest's
//! error callback is called, as AAudio does), or when the instance is shutting down.
//!
//! # An input stream is `AAUDIO_ERROR_UNAVAILABLE`
//!
//! FMOD also opens an **input** stream (its recording path, `setDirection(b, INPUT)` at
//! `0x4fbf7fc`) -- MEASURED in gate118, right after its output. This runtime has no audio input
//! device, and `AAUDIO_ERROR_UNAVAILABLE` is what AAudio answers when there is none to open: the
//! answer `dlopen`'s NULL is for a library that is not there, a fact FMOD has a branch for. A
//! refusal here killed the engine's game thread in gate118, over a microphone nothing had asked to
//! record from.
//!
//! # What is refused rather than answered
//!
//! `AAudioStream_read`, which only an input stream serves and none opens. Starting a stream with
//! **no data callback**, because the
//! blocking-write API it would need is not exported. A `dlsym` of any other `AAudio*` name, in
//! `bionic::dl`, because a real `libaaudio.so` does export it and a NULL would be a false statement
//! about the platform. Each names what it would have had to invent.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_cpu::RunLimit;
use omni_mem::{CommitPolicy, GuestAddr, Placement, Protection};
use parking_lot::{Condvar, Mutex};

use crate::boundary::{BoundaryBuilder, GuestArg, ReentrantCall, ReentrantFn};
use crate::error::{AbiError, AbiResult};

/// The `soname` FMOD opens (`0x2e2bac`), and the only one.
pub const SONAMES: [&str; 1] = ["libaaudio.so"];

/// The export whose binding makes `dlopen("libaaudio.so")` answer a handle.
pub const ENTRY_POINT: &str = "AAudio_createStreamBuilder";

/// Everything this library exports: FMOD's `dlsym` list at `0x4fbf3f8`-`0x4fbf6ec`, in its order.
pub const EXPORTS: [&str; 26] = [
    "AAudio_createStreamBuilder",
    "AAudioStreamBuilder_openStream",
    "AAudioStreamBuilder_setBufferCapacityInFrames",
    "AAudioStreamBuilder_setDirection",
    "AAudioStreamBuilder_setPerformanceMode",
    "AAudioStreamBuilder_setDataCallback",
    "AAudioStreamBuilder_setErrorCallback",
    "AAudioStreamBuilder_delete",
    "AAudioStream_getFramesPerBurst",
    "AAudioStream_getBufferCapacityInFrames",
    "AAudioStream_getBufferSizeInFrames",
    "AAudioStream_setBufferSizeInFrames",
    "AAudioStream_getFormat",
    "AAudioStream_getSampleRate",
    "AAudioStream_getState",
    "AAudioStream_getChannelCount",
    "AAudioStream_getXRunCount",
    "AAudioStream_requestStart",
    "AAudioStream_requestPause",
    "AAudioStream_requestStop",
    "AAudioStream_close",
    "AAudioStream_read",
    "AAudioStream_waitForStateChange",
    "AAudioStreamBuilder_setFormat",
    "AAudioStreamBuilder_setUsage",
    "AAudioStreamBuilder_setInputPreset",
];

/// Whether `name` is something this `libaaudio.so` exports.
#[must_use]
pub fn exports(name: &str) -> bool {
    EXPORTS.contains(&name)
}

/// The data-callback thread's start routine: a thunk, bound under a name no guest asks for.
pub const THREAD_ENTRY: &str = "__omni_aaudio_data_thread";

/// The data-area symbol whose slots are the builder and stream handles.
const HANDLES_SYMBOL: &str = "__omni_aaudio_handles";

/// How many builders one instance holds at once. FMOD holds one, briefly, per stream it opens.
pub const MAX_BUILDERS: usize = 8;

/// How many streams one instance holds at once. FMOD opens a probe and then its stream, and
/// closes the probe first; a recording stream would be a third.
pub const MAX_STREAMS: usize = 8;

/// Bytes per handle slot. The stream's slot also holds its callback thread's `pthread_t`, which
/// `pthread_create` writes there.
const HANDLE_BYTES: usize = 16;

/// How many thunk slots [`AAudio::bind_into`] takes: every export and the thread entry.
pub const BOUND_SYMBOLS: usize = EXPORTS.len() + 1;

/// The bytes of boundary data area [`AAudio::bind_into`] declares: the handle slots.
pub const REQUIRED_DATA_BYTES: usize = (MAX_BUILDERS + MAX_STREAMS) * HANDLE_BYTES;

/// Guest instructions one data callback may run. FMOD's copies from its ring buffer and, when the
/// ring is short, mixes a block inline (`0x4fbfc2c`); the mix is the upper bound, and a callback
/// that runs past this is a guest that has stopped returning.
pub const PER_CALLBACK: RunLimit = RunLimit::Instructions(200_000_000);

/// How long the callback thread waits for the host device before re-checking its stream's state
/// and the instance's stop switch. Bounds how late a pause or a shutdown is noticed.
const DEVICE_WAIT: Duration = Duration::from_millis(50);

/// How long `close` waits for a running callback thread to return.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// AAudio's constants, from `<aaudio/AAudio.h>`.
pub mod consts {
    /// `AAUDIO_OK`.
    pub const OK: i32 = 0;
    /// `AAUDIO_ERROR_DISCONNECTED`: the device went away.
    pub const ERROR_DISCONNECTED: i32 = -899;
    /// `AAUDIO_ERROR_ILLEGAL_ARGUMENT`.
    pub const ERROR_ILLEGAL_ARGUMENT: i32 = -898;
    /// `AAUDIO_ERROR_INVALID_STATE`.
    pub const ERROR_INVALID_STATE: i32 = -895;
    /// `AAUDIO_ERROR_UNAVAILABLE`: no device to open.
    pub const ERROR_UNAVAILABLE: i32 = -889;
    /// `AAUDIO_ERROR_NO_FREE_HANDLES`.
    pub const ERROR_NO_FREE_HANDLES: i32 = -888;
    /// `AAUDIO_ERROR_TIMEOUT`.
    pub const ERROR_TIMEOUT: i32 = -885;
    /// `AAUDIO_ERROR_INVALID_FORMAT`.
    pub const ERROR_INVALID_FORMAT: i32 = -883;

    /// `AAUDIO_DIRECTION_OUTPUT`.
    pub const DIRECTION_OUTPUT: i32 = 0;
    /// `AAUDIO_DIRECTION_INPUT`.
    pub const DIRECTION_INPUT: i32 = 1;

    /// `AAUDIO_FORMAT_UNSPECIFIED`.
    pub const FORMAT_UNSPECIFIED: i32 = 0;
    /// `AAUDIO_FORMAT_PCM_I16`.
    pub const FORMAT_PCM_I16: i32 = 1;
    /// `AAUDIO_FORMAT_PCM_FLOAT`.
    pub const FORMAT_PCM_FLOAT: i32 = 2;

    /// `AAUDIO_PERFORMANCE_MODE_NONE`, the builder's default.
    pub const PERFORMANCE_MODE_NONE: i32 = 10;
    /// `AAUDIO_USAGE_MEDIA`, the builder's default.
    pub const USAGE_MEDIA: i32 = 1;
    /// `AAUDIO_INPUT_PRESET_VOICE_RECOGNITION`, the builder's default.
    pub const INPUT_PRESET_VOICE_RECOGNITION: i32 = 6;

    /// `AAUDIO_STREAM_STATE_OPEN`.
    pub const STATE_OPEN: i32 = 2;
    /// `AAUDIO_STREAM_STATE_STARTING`.
    pub const STATE_STARTING: i32 = 3;
    /// `AAUDIO_STREAM_STATE_STARTED`.
    pub const STATE_STARTED: i32 = 4;
    /// `AAUDIO_STREAM_STATE_PAUSING`.
    pub const STATE_PAUSING: i32 = 5;
    /// `AAUDIO_STREAM_STATE_PAUSED`.
    pub const STATE_PAUSED: i32 = 6;
    /// `AAUDIO_STREAM_STATE_STOPPING`.
    pub const STATE_STOPPING: i32 = 9;
    /// `AAUDIO_STREAM_STATE_STOPPED`.
    pub const STATE_STOPPED: i32 = 10;
    /// `AAUDIO_STREAM_STATE_CLOSING`.
    pub const STATE_CLOSING: i32 = 11;
    /// `AAUDIO_STREAM_STATE_DISCONNECTED`.
    pub const STATE_DISCONNECTED: i32 = 13;

    /// `AAUDIO_CALLBACK_RESULT_STOP`: the data callback's "no more".
    pub const CALLBACK_RESULT_STOP: i32 = 1;
}

// ================================================================== the host side: a seam

/// **The host's audio output, as an embedding supplies it** -- the seam, with no default.
///
/// [`PlatformOutput`] is the host's real default device, through `omni-platform`. An embedding
/// that binds this module without one has made a decision, and a test supplies its own.
pub trait OutputDevice: Send + Sync {
    /// Open an output stream asking for a buffer of about `buffer_frames` frames (0: the device's
    /// own minimum), not started.
    ///
    /// # Errors
    ///
    /// A description of why the host could not open one -- no device, the device's format, an OS
    /// failure. `openStream` answers it with `AAUDIO_ERROR_UNAVAILABLE`, which is true.
    fn open(&self, buffer_frames: u32) -> Result<Box<dyn OutputSink>, String>;
}

/// One open host output stream: interleaved `f32` at the device's own rate and channel count.
pub trait OutputSink: Send {
    /// Frames per second.
    fn sample_rate(&self) -> u32;
    /// Samples per frame.
    fn channels(&self) -> u16;
    /// The host buffer's size in frames.
    fn buffer_frames(&self) -> u32;
    /// The device's period in frames.
    fn period_frames(&self) -> u32;
    /// Frames writable now.
    ///
    /// # Errors
    ///
    /// The host's failure, described.
    fn writable_frames(&self) -> Result<u32, String>;
    /// Wait until the device wants data or `timeout` passes, then answer
    /// [`writable_frames`](OutputSink::writable_frames).
    ///
    /// # Errors
    ///
    /// The host's failure, described.
    fn wait_writable(&self, timeout: Duration) -> Result<u32, String>;
    /// Append interleaved samples, no more frames than are writable.
    ///
    /// # Errors
    ///
    /// The host's failure, described.
    fn write(&mut self, samples: &[f32]) -> Result<(), String>;
    /// Start the device consuming.
    ///
    /// # Errors
    ///
    /// The host's failure, described.
    fn start(&mut self) -> Result<(), String>;
    /// Stop it.
    ///
    /// # Errors
    ///
    /// The host's failure, described.
    fn stop(&mut self) -> Result<(), String>;
}

// ================================================================== the instance

/// One `libaaudio.so`: its builders, its streams, and the device they open.
///
/// Its own instance with its own activation, for [`Vulkan`](crate::Vulkan)'s reason: a handler is
/// a bare `fn`, so per-instance state reaches it only through a guard published to the thread.
pub struct AAudio {
    device: Arc<dyn OutputDevice>,
    state: Mutex<State>,
    /// Signalled on every stream state change, for `waitForStateChange` and `close`.
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// The first handle slot, set by [`AAudio::bind_into`].
    handles: Option<GuestAddr>,
    /// [`THREAD_ENTRY`]'s thunk.
    thread_entry: Option<GuestAddr>,
    builders: BTreeMap<usize, Builder>,
    streams: BTreeMap<usize, Stream>,
    /// Calls per export, for [`AAudio::report`].
    calls: BTreeMap<&'static str, u64>,
    /// What happened to each stream, oldest first and bounded, for [`AAudio::report`].
    events: Vec<String>,
}

/// What `AAudioStreamBuilder_set*` recorded.
#[derive(Debug, Clone, Copy)]
struct Builder {
    direction: i32,
    format: i32,
    performance_mode: i32,
    usage: i32,
    input_preset: i32,
    capacity: i32,
    data_callback: (u64, u64),
    error_callback: (u64, u64),
}

impl Default for Builder {
    /// AAudio's own defaults for a new builder.
    fn default() -> Self {
        Self {
            direction: consts::DIRECTION_OUTPUT,
            format: consts::FORMAT_UNSPECIFIED,
            performance_mode: consts::PERFORMANCE_MODE_NONE,
            usage: consts::USAGE_MEDIA,
            input_preset: consts::INPUT_PRESET_VOICE_RECOGNITION,
            capacity: 0,
            data_callback: (0, 0),
            error_callback: (0, 0),
        }
    }
}

/// One open stream.
struct Stream {
    format: i32,
    sample_rate: i32,
    channels: i32,
    burst: i32,
    capacity: i32,
    buffer_size: i32,
    data_callback: (u64, u64),
    error_callback: (u64, u64),
    state: i32,
    xruns: i32,
    sink: Arc<Mutex<Box<dyn OutputSink>>>,
    /// The guest buffer each data callback writes into: base and length.
    audio: (GuestAddr, usize),
    /// Whether its callback thread is running, and which host thread that is.
    thread: Option<std::thread::ThreadId>,
    running: bool,
}

impl AAudio {
    /// A `libaaudio.so` over `device`.
    #[must_use]
    pub fn new(device: Arc<dyn OutputDevice>) -> Arc<Self> {
        Arc::new(Self { device, state: Mutex::new(State::default()), changed: Condvar::new() })
    }

    /// Bind every export, the callback thread's entry and the handle slots into `builder`.
    ///
    /// # Errors
    ///
    /// A second binding, and whatever the builder refuses.
    pub fn bind_into(&self, builder: &BoundaryBuilder) -> AbiResult<()> {
        let mut state = self.state.lock();
        if state.handles.is_some() {
            return Err(AbiError::Refused {
                symbol: "AAudio::bind_into".to_string(),
                address: 0,
                why: "this AAudio instance is already bound into a boundary; its handles would \
                      name slots in the first one's data area"
                    .to_string(),
            });
        }
        for (name, handler) in HANDLERS {
            builder.bind_reentrant(name, *handler)?;
        }
        state.thread_entry = Some(builder.bind_reentrant(THREAD_ENTRY, data_thread as ReentrantFn)?);
        state.handles = Some(builder.declare_data(
            HANDLES_SYMBOL,
            (MAX_BUILDERS + MAX_STREAMS) * HANDLE_BYTES,
            HANDLE_BYTES,
        )?);
        Ok(())
    }

    /// Publish this instance to the calling thread until the guard is dropped.
    #[must_use]
    pub fn activate(self: &Arc<Self>) -> AAudioActivation {
        let previous = ACTIVE.with(|cell| cell.borrow_mut().replace(Arc::clone(self)));
        AAudioActivation { previous }
    }

    /// This instance, as something a created guest thread carries. **An embedding that binds
    /// this module must pass it to
    /// [`ThreadHost::with_instance`](crate::bionic::ThreadHost::with_instance)**: FMOD opens its
    /// output from the engine's game thread, and the data callback runs on a thread this module
    /// creates.
    #[must_use]
    pub fn thread_instance(self: &Arc<Self>) -> Arc<dyn crate::bionic::ThreadLocalInstance> {
        Arc::new(AAudioThreadInstance(Arc::clone(self)))
    }

    /// Every stream's state and what happened to it, and the calls per export: what a gate
    /// prints.
    #[must_use]
    pub fn report(&self) -> String {
        let state = self.state.lock();
        let streams: Vec<String> = state
            .streams
            .values()
            .map(|s| {
                format!(
                    "state {} format {} {} Hz x{} burst {} size {}/{} xruns {}",
                    s.state, s.format, s.sample_rate, s.channels, s.burst, s.buffer_size,
                    s.capacity, s.xruns
                )
            })
            .collect();
        format!(
            "calls {:?}; open streams {streams:?}; events {:?}",
            state.calls, state.events
        )
    }

    /// How many times `name` was called.
    #[must_use]
    pub fn calls(&self, name: &str) -> u64 {
        self.state.lock().calls.iter().find(|(k, _)| **k == name).map_or(0, |(_, v)| *v)
    }
}

impl State {
    fn count(&mut self, name: &'static str) {
        *self.calls.entry(name).or_insert(0) += 1;
    }

    fn event(&mut self, text: String) {
        const MAX_EVENTS: usize = 64;
        if self.events.len() == MAX_EVENTS {
            self.events.remove(0);
        }
        self.events.push(text);
    }

    fn handle(&self, index: usize) -> u64 {
        self.handles.map_or(0, |base| (base + index * HANDLE_BYTES) as u64)
    }

    /// The slot index a handle names, if it is one of ours at all.
    fn slot_of(&self, handle: u64) -> Option<usize> {
        let base = self.handles? as u64;
        let offset = handle.checked_sub(base)?;
        let index = usize::try_from(offset / HANDLE_BYTES as u64).ok()?;
        (offset % HANDLE_BYTES as u64 == 0 && index < MAX_BUILDERS + MAX_STREAMS).then_some(index)
    }
}

thread_local! {
    static ACTIVE: std::cell::RefCell<Option<Arc<AAudio>>> = const { std::cell::RefCell::new(None) };
}

/// Restores the previously published instance when dropped.
pub struct AAudioActivation {
    previous: Option<Arc<AAudio>>,
}

impl Drop for AAudioActivation {
    fn drop(&mut self) {
        ACTIVE.with(|cell| *cell.borrow_mut() = self.previous.take());
    }
}

impl core::fmt::Debug for AAudioActivation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AAudioActivation")
    }
}

/// An [`AAudio`] as something a created guest thread carries.
struct AAudioThreadInstance(Arc<AAudio>);

impl crate::bionic::ThreadLocalInstance for AAudioThreadInstance {
    fn name(&self) -> &'static str {
        "AAudio"
    }

    fn publish(&self) -> AbiResult<Box<dyn core::any::Any>> {
        Ok(Box::new(self.0.activate()))
    }
}

fn active(c: &ReentrantCall<'_>) -> AbiResult<Arc<AAudio>> {
    ACTIVE.with(|cell| cell.borrow().clone()).ok_or_else(|| AbiError::Refused {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: "no AAudio instance is published on this thread: an embedding that binds \
              `libaaudio.so` must activate it on the threads that call it and pass \
              `AAudio::thread_instance` to its thread host"
            .to_string(),
    })
}

fn refuse(c: &ReentrantCall<'_>, why: String) -> AbiError {
    AbiError::Refused { symbol: c.symbol().to_string(), address: c.address(), why }
}

// ================================================================== the handlers

/// Every export and its handler. All are on the exit path: `requestStart` calls guest code, the
/// stream calls map and unmap guest memory, and none of them is hot.
const HANDLERS: &[(&str, ReentrantFn)] = &[
    ("AAudio_createStreamBuilder", create_stream_builder),
    ("AAudioStreamBuilder_openStream", open_stream),
    ("AAudioStreamBuilder_setBufferCapacityInFrames", set_buffer_capacity),
    ("AAudioStreamBuilder_setDirection", set_direction),
    ("AAudioStreamBuilder_setPerformanceMode", set_performance_mode),
    ("AAudioStreamBuilder_setDataCallback", set_data_callback),
    ("AAudioStreamBuilder_setErrorCallback", set_error_callback),
    ("AAudioStreamBuilder_delete", delete_builder),
    ("AAudioStream_getFramesPerBurst", get_frames_per_burst),
    ("AAudioStream_getBufferCapacityInFrames", get_buffer_capacity),
    ("AAudioStream_getBufferSizeInFrames", get_buffer_size),
    ("AAudioStream_setBufferSizeInFrames", set_buffer_size),
    ("AAudioStream_getFormat", get_format),
    ("AAudioStream_getSampleRate", get_sample_rate),
    ("AAudioStream_getState", get_state),
    ("AAudioStream_getChannelCount", get_channel_count),
    ("AAudioStream_getXRunCount", get_xrun_count),
    ("AAudioStream_requestStart", request_start),
    ("AAudioStream_requestPause", request_pause),
    ("AAudioStream_requestStop", request_stop),
    ("AAudioStream_close", close),
    ("AAudioStream_read", read),
    ("AAudioStream_waitForStateChange", wait_for_state_change),
    ("AAudioStreamBuilder_setFormat", set_format),
    ("AAudioStreamBuilder_setUsage", set_usage),
    ("AAudioStreamBuilder_setInputPreset", set_input_preset),
];

/// The export this call is, as the `'static` name its census counts it under.
fn export_name(c: &ReentrantCall<'_>) -> &'static str {
    EXPORTS.iter().copied().find(|name| *name == c.symbol()).unwrap_or(THREAD_ENTRY)
}

/// The builder a handle names, or a refusal: a handle this layer never issued is not a builder.
fn with_builder<T>(
    c: &ReentrantCall<'_>,
    audio: &AAudio,
    handle: u64,
    f: impl FnOnce(&mut Builder) -> T,
) -> AbiResult<T> {
    let mut state = audio.state.lock();
    state.count(export_name(c));
    let slot = state.slot_of(handle).filter(|&slot| slot < MAX_BUILDERS);
    match slot.and_then(|slot| state.builders.get_mut(&slot)) {
        Some(builder) => Ok(f(builder)),
        None => Err(refuse(c, format!("{handle:#x} is not a live AAudioStreamBuilder this layer issued"))),
    }
}

/// The stream a handle names, or a refusal.
fn with_stream<T>(
    c: &ReentrantCall<'_>,
    audio: &AAudio,
    handle: u64,
    f: impl FnOnce(&mut Stream) -> T,
) -> AbiResult<T> {
    let mut state = audio.state.lock();
    state.count(export_name(c));
    let slot = state.slot_of(handle).filter(|&slot| slot >= MAX_BUILDERS);
    match slot.and_then(|slot| state.streams.get_mut(&slot)) {
        Some(stream) => Ok(f(stream)),
        None => Err(refuse(c, format!("{handle:#x} is not a live AAudioStream this layer issued"))),
    }
}

/// `aaudio_result_t AAudio_createStreamBuilder(AAudioStreamBuilder **builder)`
fn create_stream_builder(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let out = c.args().next_u64()?;
    let audio = active(c)?;
    let blame = c.blame(0);
    let result = {
        let mut state = audio.state.lock();
        state.count("AAudio_createStreamBuilder");
        match (0..MAX_BUILDERS).find(|slot| !state.builders.contains_key(slot)) {
            None => consts::ERROR_NO_FREE_HANDLES,
            Some(slot) => {
                let handle = state.handle(slot);
                c.mem().write_u64(out as GuestAddr, handle, blame)?;
                state.builders.insert(slot, Builder::default());
                consts::OK
            }
        }
    };
    c.ret(|mut r| r.i32(result));
    Ok(())
}

/// A `void AAudioStreamBuilder_set*(AAudioStreamBuilder *builder, int32_t value)`.
fn set_builder_int(c: &mut ReentrantCall<'_>, apply: fn(&mut Builder, i32)) -> AbiResult<()> {
    let (handle, value) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_i32()?)
    };
    let audio = active(c)?;
    with_builder(c, &audio, handle, |builder| apply(builder, value))
}

fn set_buffer_capacity(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    set_builder_int(c, |b, v| b.capacity = v)
}

fn set_direction(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    set_builder_int(c, |b, v| b.direction = v)
}

fn set_performance_mode(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    set_builder_int(c, |b, v| b.performance_mode = v)
}

fn set_format(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    set_builder_int(c, |b, v| b.format = v)
}

fn set_usage(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    set_builder_int(c, |b, v| b.usage = v)
}

fn set_input_preset(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    set_builder_int(c, |b, v| b.input_preset = v)
}

/// `void AAudioStreamBuilder_setDataCallback(b, AAudioStream_dataCallback callback, void *user)`
fn set_data_callback(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (handle, callback, user) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let audio = active(c)?;
    with_builder(c, &audio, handle, |builder| builder.data_callback = (callback, user))
}

/// `void AAudioStreamBuilder_setErrorCallback(b, AAudioStream_errorCallback callback, void *user)`
fn set_error_callback(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (handle, callback, user) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let audio = active(c)?;
    with_builder(c, &audio, handle, |builder| builder.error_callback = (callback, user))
}

/// `aaudio_result_t AAudioStreamBuilder_delete(AAudioStreamBuilder *builder)`
fn delete_builder(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let handle = c.args().next_u64()?;
    let audio = active(c)?;
    with_builder(c, &audio, handle, |_| ())?;
    {
        let mut state = audio.state.lock();
        if let Some(slot) = state.slot_of(handle) {
            state.builders.remove(&slot);
        }
    }
    c.ret(|mut r| r.i32(consts::OK));
    Ok(())
}

/// The format a stream opens with: the builder's, or -- unspecified -- the host mix format's
/// sample type, which is `float`. `None` for one this stream cannot carry.
fn stream_format(requested: i32) -> Option<i32> {
    match requested {
        consts::FORMAT_UNSPECIFIED | consts::FORMAT_PCM_FLOAT => Some(consts::FORMAT_PCM_FLOAT),
        consts::FORMAT_PCM_I16 => Some(consts::FORMAT_PCM_I16),
        _ => None,
    }
}

/// Bytes per sample of a stream format this module carries.
fn sample_bytes(format: i32) -> usize {
    if format == consts::FORMAT_PCM_I16 { 2 } else { 4 }
}

/// `aaudio_result_t AAudioStreamBuilder_openStream(AAudioStreamBuilder *b, AAudioStream **stream)`
fn open_stream(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (handle, out) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let audio = active(c)?;
    let builder = with_builder(c, &audio, handle, |builder| *builder)?;
    if builder.direction == consts::DIRECTION_INPUT {
        // See the module documentation: there is no input device, and this is AAudio's answer
        // for that.
        audio.state.lock().event("openStream: an input stream -- no audio input device".to_string());
        c.ret(|mut r| r.i32(consts::ERROR_UNAVAILABLE));
        return Ok(());
    }
    let Some(format) = stream_format(builder.format) else {
        c.ret(|mut r| r.i32(consts::ERROR_INVALID_FORMAT));
        return Ok(());
    };
    let requested = u32::try_from(builder.capacity).unwrap_or(0);
    let sink = match audio.device.open(requested) {
        Ok(sink) => sink,
        Err(why) => {
            audio.state.lock().event(format!("openStream: the host device did not open: {why}"));
            c.ret(|mut r| r.i32(consts::ERROR_UNAVAILABLE));
            return Ok(());
        }
    };
    let (rate, channels, capacity, burst) =
        (sink.sample_rate(), sink.channels(), sink.buffer_frames(), sink.period_frames());
    let as_i32 = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
    let bytes = (capacity as usize) * usize::from(channels) * sample_bytes(format);
    let page = c.mem().space().page_size();
    let length = bytes.div_ceil(page).max(1) * page;
    let Ok(base) = c.mem().space().map_anonymous(
        Placement::Anywhere { align: page },
        length,
        Protection::ReadWrite,
        CommitPolicy::Eager,
    ) else {
        c.ret(|mut r| r.i32(consts::ERROR_NO_FREE_HANDLES));
        return Ok(());
    };
    let blame = c.blame(1);
    let result = {
        let mut state = audio.state.lock();
        match (MAX_BUILDERS..MAX_BUILDERS + MAX_STREAMS).find(|slot| !state.streams.contains_key(slot)) {
            None => None,
            Some(slot) => {
                let stream_handle = state.handle(slot);
                if let Err(error) = c.mem().write_u64(out as GuestAddr, stream_handle, blame) {
                    drop(state);
                    let _ = c.mem().space().unmap(base, length);
                    return Err(error);
                }
                state.event(format!(
                    "openStream {stream_handle:#x}: {rate} Hz x{channels}, format {format}, burst \
                     {burst}, capacity {capacity} (asked {requested}), performance mode {}, usage {}",
                    builder.performance_mode, builder.usage
                ));
                state.streams.insert(
                    slot,
                    Stream {
                        format,
                        sample_rate: as_i32(rate),
                        channels: i32::from(channels),
                        burst: as_i32(burst),
                        capacity: as_i32(capacity),
                        buffer_size: as_i32(capacity),
                        data_callback: builder.data_callback,
                        error_callback: builder.error_callback,
                        state: consts::STATE_OPEN,
                        xruns: 0,
                        sink: Arc::new(Mutex::new(sink)),
                        audio: (base, length),
                        thread: None,
                        running: false,
                    },
                );
                Some(())
            }
        }
    };
    if result.is_none() {
        let _ = c.mem().space().unmap(base, length);
        c.ret(|mut r| r.i32(consts::ERROR_NO_FREE_HANDLES));
        return Ok(());
    }
    c.ret(|mut r| r.i32(consts::OK));
    Ok(())
}

/// A `T AAudioStream_get*(AAudioStream *stream)` answered from the stream.
fn get_int(c: &mut ReentrantCall<'_>, read: fn(&Stream) -> i32) -> AbiResult<()> {
    let handle = c.args().next_u64()?;
    let audio = active(c)?;
    let value = with_stream(c, &audio, handle, |stream| read(stream))?;
    c.ret(|mut r| r.i32(value));
    Ok(())
}

fn get_frames_per_burst(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    get_int(c, |s| s.burst)
}

fn get_buffer_capacity(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    get_int(c, |s| s.capacity)
}

fn get_buffer_size(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    get_int(c, |s| s.buffer_size)
}

fn get_format(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    get_int(c, |s| s.format)
}

fn get_sample_rate(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    get_int(c, |s| s.sample_rate)
}

fn get_state(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    get_int(c, |s| s.state)
}

fn get_channel_count(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    get_int(c, |s| s.channels)
}

fn get_xrun_count(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    get_int(c, |s| s.xruns)
}

/// The buffer size a request becomes: at least one burst -- below it no callback could ever be
/// made, since each asks for a burst -- and at most the capacity. AAudio clips the same way.
fn clipped_buffer_size(requested: i32, burst: i32, capacity: i32) -> i32 {
    requested.clamp(burst.min(capacity), capacity)
}

/// `aaudio_result_t AAudioStream_setBufferSizeInFrames(AAudioStream *stream, int32_t frames)`:
/// the size it became.
fn set_buffer_size(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (handle, frames) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_i32()?)
    };
    let audio = active(c)?;
    let size = with_stream(c, &audio, handle, |stream| {
        stream.buffer_size = clipped_buffer_size(frames, stream.burst, stream.capacity);
        stream.buffer_size
    })?;
    c.ret(|mut r| r.i32(size));
    Ok(())
}

/// `aaudio_result_t AAudioStream_requestStart(AAudioStream *stream)`
///
/// Starts the data-callback thread through the guest's own `pthread_create` and detaches it; see
/// the module documentation for why that thread must be a guest thread.
fn request_start(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let handle = c.args().next_u64()?;
    let audio = active(c)?;
    let (entry, previous) = {
        let mut state = audio.state.lock();
        state.count("AAudioStream_requestStart");
        let entry = state.thread_entry;
        let slot = state.slot_of(handle).filter(|&slot| slot >= MAX_BUILDERS);
        let Some(stream) = slot.and_then(|slot| state.streams.get_mut(&slot)) else {
            return Err(refuse(c, format!("{handle:#x} is not a live AAudioStream this layer issued")));
        };
        if !matches!(stream.state, consts::STATE_OPEN | consts::STATE_PAUSED | consts::STATE_STOPPED)
            || stream.running
        {
            drop(state);
            c.ret(|mut r| r.i32(consts::ERROR_INVALID_STATE));
            return Ok(());
        }
        if stream.data_callback.0 == 0 {
            return Err(refuse(
                c,
                "a stream with no data callback was started. It would be fed by \
                 `AAudioStream_write`, which this library does not export because FMOD does not \
                 fetch it"
                    .to_string(),
            ));
        }
        let previous = stream.state;
        stream.state = consts::STATE_STARTING;
        stream.running = true;
        (entry, previous)
    };
    let boundary = Arc::clone(c.boundary());
    let thunk = |name: &str| boundary.lookup(None, name).map(|slot| slot.address);
    let (Some(create), Some(detach), Some(entry)) =
        (thunk("pthread_create"), thunk("pthread_detach"), entry)
    else {
        return Err(refuse(
            c,
            "the data-callback thread is created through the guest's `pthread_create` and \
             `pthread_detach`, and this boundary has one of them (or this module's thread entry) \
             unbound"
                .to_string(),
        ));
    };
    // `pthread_create` writes the new thread's `pthread_t` into the stream's own handle slot.
    let out = handle as GuestAddr;
    let created = c.call_guest(
        create,
        &[GuestArg::Pointer(out), GuestArg::Int(0), GuestArg::Pointer(entry), GuestArg::Int(handle)],
        RunLimit::Instructions(10_000_000),
    )?;
    if created.as_i32() != 0 {
        let mut state = audio.state.lock();
        if let Some(stream) = state.slot_of(handle).and_then(|slot| state.streams.get_mut(&slot)) {
            stream.state = previous;
            stream.running = false;
        }
        drop(state);
        return Err(refuse(
            c,
            format!(
                "the guest's `pthread_create` answered {} for the data-callback thread",
                created.as_i32()
            ),
        ));
    }
    let thread = c.mem().read_u64(out, c.blame(0))?;
    let detached = c.call_guest(detach, &[GuestArg::Int(thread)], RunLimit::Instructions(1_000_000))?;
    if detached.as_i32() != 0 {
        return Err(refuse(
            c,
            format!("the guest's `pthread_detach` answered {} for the data-callback thread", detached.as_i32()),
        ));
    }
    audio.state.lock().event(format!("requestStart {handle:#x}: data-callback thread {thread:#x}"));
    c.ret(|mut r| r.i32(consts::OK));
    Ok(())
}

/// `requestPause`/`requestStop`: a started stream's thread is told, and the call returns -- AAudio
/// is asynchronous here, and the thread moves the state on when it has stopped.
fn request_halt(c: &mut ReentrantCall<'_>, transient: i32, settled: i32) -> AbiResult<()> {
    let handle = c.args().next_u64()?;
    let audio = active(c)?;
    let result = with_stream(c, &audio, handle, |stream| {
        if stream.running {
            stream.state = transient;
            consts::OK
        } else if matches!(stream.state, consts::STATE_OPEN | consts::STATE_PAUSED | consts::STATE_STOPPED) {
            stream.state = settled;
            consts::OK
        } else {
            consts::ERROR_INVALID_STATE
        }
    })?;
    audio.changed.notify_all();
    c.ret(|mut r| r.i32(result));
    Ok(())
}

fn request_pause(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    request_halt(c, consts::STATE_PAUSING, consts::STATE_PAUSED)
}

fn request_stop(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    request_halt(c, consts::STATE_STOPPING, consts::STATE_STOPPED)
}

/// `aaudio_result_t AAudioStream_close(AAudioStream *stream)`
///
/// Stops a running callback thread and waits for it, then gives back the host stream and the
/// guest buffer.
fn close(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let handle = c.args().next_u64()?;
    let audio = active(c)?;
    let on_callback_thread = with_stream(c, &audio, handle, |stream| {
        let here = stream.thread == Some(std::thread::current().id());
        if stream.running {
            stream.state = consts::STATE_CLOSING;
        }
        here
    })?;
    if on_callback_thread {
        return Err(refuse(
            c,
            "a stream was closed from its own data-callback thread, which would have to wait for \
             itself to return"
                .to_string(),
        ));
    }
    audio.changed.notify_all();
    let deadline = Instant::now() + CLOSE_WAIT;
    let mut state = audio.state.lock();
    let slot = state.slot_of(handle);
    loop {
        let running = slot.and_then(|slot| state.streams.get(&slot)).is_some_and(|s| s.running);
        if !running {
            break;
        }
        if audio.changed.wait_until(&mut state, deadline).timed_out() {
            drop(state);
            return Err(refuse(
                c,
                format!("the data-callback thread of {handle:#x} did not return within {CLOSE_WAIT:?} of close"),
            ));
        }
    }
    let stream = slot.and_then(|slot| state.streams.remove(&slot));
    state.event(format!("close {handle:#x}"));
    drop(state);
    if let Some(stream) = stream {
        let _ = stream.sink.lock().stop();
        let _ = c.mem().space().unmap(stream.audio.0, stream.audio.1);
    }
    c.ret(|mut r| r.i32(consts::OK));
    Ok(())
}

/// `aaudio_result_t AAudioStream_read(AAudioStream *stream, void *buffer, int32_t frames, int64_t
/// timeoutNanoseconds)`: an input stream's, and this runtime opens none.
fn read(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let audio = active(c)?;
    audio.state.lock().count("AAudioStream_read");
    Err(refuse(
        c,
        "`AAudioStream_read` reads an input stream, and this runtime opens none -- there is no \
         audio input behind it"
            .to_string(),
    ))
}

/// `aaudio_result_t AAudioStream_waitForStateChange(AAudioStream *stream, aaudio_stream_state_t
/// inputState, aaudio_stream_state_t *nextState, int64_t timeoutNanoseconds)`
fn wait_for_state_change(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (handle, input, next, timeout) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    let audio = active(c)?;
    with_stream(c, &audio, handle, |_| ())?;
    let bionic = crate::bionic::active(c.symbol(), c.address())?.bionic;
    let deadline = Instant::now() + Duration::from_nanos(timeout);
    let mut state = audio.state.lock();
    let slot = state.slot_of(handle);
    let (result, now) = loop {
        let now = slot.and_then(|slot| state.streams.get(&slot)).map_or(consts::STATE_CLOSING, |s| s.state);
        if now != input {
            break (consts::OK, now);
        }
        if Instant::now() >= deadline || bionic.guest_threads_stopping() {
            break (consts::ERROR_TIMEOUT, now);
        }
        let _ = audio.changed.wait_until(&mut state, deadline.min(Instant::now() + DEVICE_WAIT));
    };
    drop(state);
    if next != 0 {
        c.mem().write_u32(next as GuestAddr, now as u32, c.blame(2))?;
    }
    c.ret(|mut r| r.i32(result));
    Ok(())
}

// ================================================================== the data-callback thread

/// Samples a callback wrote, as the `f32` the host takes.
fn to_f32(format: i32, bytes: &[u8]) -> Vec<f32> {
    if format == consts::FORMAT_PCM_I16 {
        bytes
            .chunks_exact(2)
            .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
            .collect()
    } else {
        bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
    }
}

/// The frames the callback thread may ask for now: what the stream's buffer size allows beyond
/// what the host already holds (`buffer_frames - writable`).
fn allowed_frames(buffer_size: i32, buffer_frames: u32, writable: u32) -> u32 {
    let held = buffer_frames.saturating_sub(writable);
    u32::try_from(buffer_size).unwrap_or(0).saturating_sub(held)
}

/// [`THREAD_ENTRY`]: `void *start_routine(void *stream)`, on the guest thread `requestStart`
/// created. See the module documentation.
fn data_thread(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let handle = c.args().next_u64()?;
    let audio = active(c)?;
    let bionic = crate::bionic::active(c.symbol(), c.address())?.bionic;
    let found = {
        let mut state = audio.state.lock();
        let slot = state.slot_of(handle).filter(|&slot| slot >= MAX_BUILDERS);
        slot.and_then(|slot| state.streams.get_mut(&slot)).map(|stream| {
            stream.thread = Some(std::thread::current().id());
            if stream.state == consts::STATE_STARTING {
                stream.state = consts::STATE_STARTED;
            }
            (
                Arc::clone(&stream.sink),
                stream.data_callback,
                stream.error_callback,
                stream.audio.0,
                stream.burst,
                stream.channels,
                stream.format,
            )
        })
    };
    audio.changed.notify_all();
    let Some((sink, (callback, user), (error_callback, error_user), buffer, burst, channels, format)) = found
    else {
        return Err(refuse(c, format!("the data-callback thread was started for {handle:#x}, which is not a live stream")));
    };
    let burst_frames = u32::try_from(burst).unwrap_or(0).max(1);
    let burst_bytes = burst_frames as usize * usize::try_from(channels).unwrap_or(0) * sample_bytes(format);
    let mut primed = false;
    let mut outcome: AbiResult<()> = Ok(());
    let mut disconnected = false;
    let mut stopped_by_callback = false;
    'feed: loop {
        let buffer_size = {
            let state = audio.state.lock();
            match state.slot_of(handle).and_then(|slot| state.streams.get(&slot)) {
                Some(stream) if stream.state == consts::STATE_STARTED => stream.buffer_size,
                _ => break 'feed,
            }
        };
        if bionic.guest_threads_stopping() {
            break;
        }
        // An unstarted host stream neither drains nor signals, so the first fill asks for the
        // free space rather than waiting out the timeout for a signal that cannot come.
        let waited = if primed {
            sink.lock().wait_writable(DEVICE_WAIT)
        } else {
            sink.lock().writable_frames()
        };
        let writable = match waited {
            Ok(frames) => frames,
            Err(why) => {
                audio.state.lock().event(format!("{handle:#x}: the host device failed: {why}"));
                disconnected = true;
                break;
            }
        };
        let buffer_frames = sink.lock().buffer_frames();
        if primed && writable >= buffer_frames {
            let mut state = audio.state.lock();
            if let Some(stream) = state.slot_of(handle).and_then(|slot| state.streams.get_mut(&slot)) {
                stream.xruns = stream.xruns.saturating_add(1);
            }
        }
        let mut allowed = allowed_frames(buffer_size, buffer_frames, writable).min(writable);
        let mut wrote = false;
        while allowed >= burst_frames {
            let returned = match c.call_guest(
                callback as GuestAddr,
                &[
                    GuestArg::Int(handle),
                    GuestArg::Int(user),
                    GuestArg::Pointer(buffer),
                    GuestArg::Int(u64::from(burst_frames)),
                ],
                PER_CALLBACK,
            ) {
                Ok(returned) => returned,
                Err(error) => {
                    outcome = Err(error);
                    break 'feed;
                }
            };
            let bytes = c.mem().read_bytes(buffer, burst_bytes, c.blame(0))?;
            if let Err(why) = sink.lock().write(&to_f32(format, &bytes)) {
                audio.state.lock().event(format!("{handle:#x}: the host device refused a write: {why}"));
                disconnected = true;
                break 'feed;
            }
            wrote = true;
            allowed -= burst_frames;
            if returned.as_i32() == consts::CALLBACK_RESULT_STOP {
                stopped_by_callback = true;
                break 'feed;
            }
        }
        if wrote && !primed {
            if let Err(why) = sink.lock().start() {
                audio.state.lock().event(format!("{handle:#x}: the host device did not start: {why}"));
                disconnected = true;
                break;
            }
            primed = true;
        }
    }
    let _ = sink.lock().stop();
    {
        let mut state = audio.state.lock();
        if let Some(stream) = state.slot_of(handle).and_then(|slot| state.streams.get_mut(&slot)) {
            stream.state = match stream.state {
                _ if disconnected => consts::STATE_DISCONNECTED,
                consts::STATE_PAUSING => consts::STATE_PAUSED,
                consts::STATE_CLOSING => consts::STATE_CLOSING,
                _ if stopped_by_callback => consts::STATE_STOPPED,
                _ => consts::STATE_STOPPED,
            };
            stream.running = false;
            stream.thread = None;
        }
    }
    audio.changed.notify_all();
    outcome?;
    // AAudio's own error path: a stream whose device went away is DISCONNECTED and its error
    // callback is told, from a thread of AAudio's -- this one.
    if disconnected && error_callback != 0 {
        c.call_guest(
            error_callback as GuestAddr,
            &[
                GuestArg::Int(handle),
                GuestArg::Int(error_user),
                GuestArg::Int(consts::ERROR_DISCONNECTED as u32 as u64),
            ],
            PER_CALLBACK,
        )?;
    }
    c.ret(|mut r| r.u64(0));
    Ok(())
}

// ================================================================== the platform's device

/// The host's default audio output, through `omni_platform::audio` -- the [`OutputDevice`] a
/// real embedding binds.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlatformOutput;

impl OutputDevice for PlatformOutput {
    fn open(&self, buffer_frames: u32) -> Result<Box<dyn OutputSink>, String> {
        omni_platform::audio::AudioOutput::open(buffer_frames)
            .map(|output| Box::new(PlatformSink(output)) as Box<dyn OutputSink>)
            .map_err(|error| error.to_string())
    }
}

struct PlatformSink(omni_platform::audio::AudioOutput);

impl OutputSink for PlatformSink {
    fn sample_rate(&self) -> u32 {
        self.0.format().sample_rate
    }
    fn channels(&self) -> u16 {
        self.0.format().channels
    }
    fn buffer_frames(&self) -> u32 {
        self.0.buffer_frames()
    }
    fn period_frames(&self) -> u32 {
        self.0.period_frames()
    }
    fn writable_frames(&self) -> Result<u32, String> {
        self.0.writable_frames().map_err(|e| e.to_string())
    }
    fn wait_writable(&self, timeout: Duration) -> Result<u32, String> {
        self.0.wait_writable(timeout).map_err(|e| e.to_string())
    }
    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        self.0.write(samples).map_err(|e| e.to_string())
    }
    fn start(&mut self) -> Result<(), String> {
        self.0.start().map_err(|e| e.to_string())
    }
    fn stop(&mut self) -> Result<(), String> {
        self.0.stop().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every export has exactly one handler, and nothing else is bound under an export's name.
    #[test]
    fn every_export_has_one_handler_and_only_exports_do() {
        for name in EXPORTS {
            assert_eq!(HANDLERS.iter().filter(|(n, _)| *n == name).count(), 1, "{name}");
        }
        assert_eq!(HANDLERS.len(), EXPORTS.len());
        assert!(exports(ENTRY_POINT));
        assert!(!exports(THREAD_ENTRY), "the thread entry is not something `dlsym` hands out");
        assert!(!exports("AAudioStream_write"), "the blocking write is not exported");
    }

    /// A callback's `int16` and `float` samples arrive as the host's `f32`, exactly.
    #[test]
    fn samples_convert_from_both_formats() {
        let i16s: Vec<u8> = [i16::MIN, -16384, 0, 16384].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(to_f32(consts::FORMAT_PCM_I16, &i16s), vec![-1.0, -0.5, 0.0, 0.5]);
        let floats: Vec<u8> = [0.25f32, -0.75].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(to_f32(consts::FORMAT_PCM_FLOAT, &floats), vec![0.25, -0.75]);
    }

    /// An unspecified format becomes the host's `float`; `int16` is carried; anything else is not.
    #[test]
    fn the_stream_format_is_the_hosts_float_unless_asked_for_int16() {
        assert_eq!(stream_format(consts::FORMAT_UNSPECIFIED), Some(consts::FORMAT_PCM_FLOAT));
        assert_eq!(stream_format(consts::FORMAT_PCM_I16), Some(consts::FORMAT_PCM_I16));
        assert_eq!(stream_format(3), None, "I24_PACKED is not carried");
    }

    /// The buffer size is clipped to [one burst, capacity], and what the callback may be asked for
    /// is the size less what the host already holds.
    #[test]
    fn buffer_size_and_allowance_follow_the_host() {
        assert_eq!(clipped_buffer_size(10, 480, 1920), 480, "at least a burst");
        assert_eq!(clipped_buffer_size(960, 480, 1920), 960);
        assert_eq!(clipped_buffer_size(5000, 480, 1920), 1920, "at most the capacity");
        // A 1920-frame host buffer holding 480 (writable 1440) with a 960 buffer size: 480 more.
        assert_eq!(allowed_frames(960, 1920, 1440), 480);
        assert_eq!(allowed_frames(960, 1920, 480), 0, "already holding more than the size");
    }
}
