//! macOS backend for the audio seam: a Core Audio output unit behind a ring buffer.
//!
//! # The shape
//!
//! Core Audio is **callback-driven**: an `AudioUnit` of subtype `kAudioUnitSubType_DefaultOutput`
//! (the default output device, followed when the user changes it) calls a render callback on its
//! real-time I/O thread each time the device wants a buffer. This seam is **pull-shaped** -- the
//! runtime's own thread asks how much fits, writes it, and waits for the device to take some -- so
//! the two meet in a single-producer, single-consumer ring of interleaved `f32` frames:
//!
//! * [`AudioOutput::write`] (the producer, `&mut self`) copies into the ring and publishes the new
//!   write position;
//! * the render callback (the consumer, the I/O thread) copies out what the device asked for,
//!   fills any shortfall with silence (an underrun: the ring had less than a period), publishes the
//!   new read position, and signals a semaphore;
//! * [`AudioOutput::writable_frames`] is the ring's free space, from the two positions;
//! * [`AudioOutput::wait_writable`] waits on the semaphore -- the device consuming a period -- or
//!   the timeout. Signals that piled up while nobody waited are collapsed after a wake, so a wait
//!   behaves like the Windows backend's auto-reset event: at most one stale wake.
//!
//! The callback takes no lock and allocates nothing: two atomics, a copy, and
//! `dispatch_semaphore_signal`, which is safe to call from a real-time thread.
//!
//! # The format: the device's, reported, and float by construction
//!
//! The unit's **output** scope is the device side, and its stream format is what the device runs
//! at: that is what [`AudioOutput::format`] reports (sample rate and channel count). The **input**
//! scope -- what the callback supplies -- is set to interleaved 32-bit float at exactly that rate
//! and channel count, so the unit converts nothing but the sample layout the device's own HAL
//! format needs. MEASURED on this host (`tests/audio_live.rs`, printed by
//! `the_default_device_opens_and_reports_a_usable_shape`): see `docs/ports/macos-window.md`.
//!
//! # Buffer and period
//!
//! The period is the device's I/O buffer size (`kAudioDevicePropertyBufferFrameSize`), the number
//! of frames one callback takes. The ring holds the frames asked for, and never fewer than **two
//! periods**, since one period is what a callback removes at once and a ring of exactly that would
//! leave the writer no time to refill before the next; `open(0)` therefore gets two periods.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::{AudioError, AudioResult, OutputFormat};

// ---------------------------------------------------------------------------------- FFI

type OsStatus = i32;
type AudioUnit = *mut c_void;

#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

type RenderCallback =
    extern "C" fn(*mut c_void, *mut u32, *const c_void, u32, u32, *mut AudioBufferList) -> OsStatus;

#[repr(C)]
struct AuRenderCallbackStruct {
    input_proc: RenderCallback,
    input_proc_ref_con: *mut c_void,
}

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(component: *mut c_void, description: *const AudioComponentDescription) -> *mut c_void;
    fn AudioComponentInstanceNew(component: *mut c_void, instance: *mut AudioUnit) -> OsStatus;
    fn AudioComponentInstanceDispose(instance: AudioUnit) -> OsStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OsStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OsStatus;
    fn AudioUnitGetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *mut c_void, size: *mut u32) -> OsStatus;
    fn AudioUnitSetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> OsStatus;
    fn AudioOutputUnitStart(unit: AudioUnit) -> OsStatus;
    fn AudioOutputUnitStop(unit: AudioUnit) -> OsStatus;
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> OsStatus;
}

unsafe extern "C" {
    fn dispatch_semaphore_create(value: isize) -> *mut c_void;
    fn dispatch_semaphore_signal(semaphore: *mut c_void) -> isize;
    fn dispatch_semaphore_wait(semaphore: *mut c_void, timeout: u64) -> isize;
    fn dispatch_time(when: u64, delta: i64) -> u64;
    fn dispatch_release(object: *mut c_void);
}

/// A four-character code, as Core Audio's headers spell them.
const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

const K_AUDIO_UNIT_TYPE_OUTPUT: u32 = fourcc(b"auou");
const K_AUDIO_UNIT_SUB_TYPE_DEFAULT_OUTPUT: u32 = fourcc(b"def ");
const K_AUDIO_UNIT_MANUFACTURER_APPLE: u32 = fourcc(b"appl");
const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
const K_AUDIO_UNIT_SCOPE_INPUT: u32 = 1;
const K_AUDIO_UNIT_SCOPE_OUTPUT: u32 = 2;
const K_AUDIO_FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 8;
const K_AUDIO_OBJECT_SYSTEM_OBJECT: u32 = 1;
const K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE: u32 = fourcc(b"dOut");
const K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz");
const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = fourcc(b"glob");
const DISPATCH_TIME_NOW: u64 = 0;

fn os(operation: &'static str, api: &'static str, status: OsStatus) -> AudioResult<()> {
    if status == 0 { Ok(()) } else { Err(AudioError::OsStatus { operation, api, status }) }
}

// ---------------------------------------------------------------------------------- the ring

/// The frames between the writer and the I/O thread. See this module's header.
struct Ring {
    /// `capacity * channels` samples. Written only in the free region (by the writer) and read
    /// only in the filled region (by the callback); the positions below keep the two apart.
    samples: Box<[UnsafeCell<f32>]>,
    capacity: u64,
    channels: usize,
    /// Frames ever written. Stored by the writer with `Release`, loaded by the callback with
    /// `Acquire`, so the callback sees the samples the position covers.
    written: AtomicU64,
    /// Frames ever consumed by the device. Stored by the callback with `Release`, loaded by the
    /// writer with `Acquire`, so the writer never overwrites a frame the callback is reading.
    consumed: AtomicU64,
    /// Signalled once per callback.
    wake: *mut c_void,
}

// SAFETY: `samples` is shared between exactly one writer and one reader, which touch disjoint
// regions delimited by the atomics (see their documentation); `wake` is a libdispatch semaphore,
// which is thread-safe.
unsafe impl Send for Ring {}
// SAFETY: as above.
unsafe impl Sync for Ring {}

impl Ring {
    fn free(&self) -> u32 {
        let queued = self.written.load(Ordering::Relaxed) - self.consumed.load(Ordering::Acquire);
        // `queued <= capacity` always (a write never exceeds the free space), and the capacity
        // fits a `u32` because it was built from one.
        (self.capacity - queued) as u32
    }

    /// Copy `frames` frames of `source` in at the write position. The caller checked they fit.
    fn push(&self, source: &[f32], frames: u32) {
        let start = self.written.load(Ordering::Relaxed);
        for frame in 0..u64::from(frames) {
            let slot = ((start + frame) % self.capacity) as usize * self.channels;
            let from = frame as usize * self.channels;
            for channel in 0..self.channels {
                // SAFETY: this slot is in the free region, which the reader does not touch.
                unsafe { *self.samples[slot + channel].get() = source[from + channel] };
            }
        }
        self.written.store(start + u64::from(frames), Ordering::Release);
    }

    /// Copy up to `frames` frames out into `out` (interleaved), silence after them. Answers how
    /// many were real.
    fn pull(&self, out: &mut [f32], frames: u64) -> u64 {
        let start = self.consumed.load(Ordering::Relaxed);
        let available = (self.written.load(Ordering::Acquire) - start).min(frames);
        for frame in 0..available {
            let slot = ((start + frame) % self.capacity) as usize * self.channels;
            let to = frame as usize * self.channels;
            for channel in 0..self.channels {
                // SAFETY: this slot is in the filled region, which the writer does not touch.
                out[to + channel] = unsafe { *self.samples[slot + channel].get() };
            }
        }
        for sample in &mut out[available as usize * self.channels..] {
            *sample = 0.0;
        }
        self.consumed.store(start + available, Ordering::Release);
        available
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: created by `dispatch_semaphore_create`; the unit that signalled it is disposed.
        unsafe { dispatch_release(self.wake) };
    }
}

/// The render callback, on Core Audio's I/O thread. No lock, no allocation.
extern "C" fn render(
    ref_con: *mut c_void,
    _flags: *mut u32,
    _time: *const c_void,
    _bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> OsStatus {
    // SAFETY: `ref_con` is the `Ring` the `AudioOutput` owns, which outlives the unit.
    let ring = unsafe { &*ref_con.cast::<Ring>() };
    // SAFETY: Core Audio hands a valid list; the input format is interleaved, so one buffer.
    let buffer = unsafe { &mut (*data).buffers[0] };
    let samples = (buffer.data_byte_size as usize / 4).min(frames as usize * ring.channels);
    // SAFETY: `data` holds `data_byte_size` bytes of `f32`s, per the format set on the unit.
    let out = unsafe { core::slice::from_raw_parts_mut(buffer.data.cast::<f32>(), samples) };
    ring.pull(out, (samples / ring.channels) as u64);
    // SAFETY: a libdispatch semaphore, safe from any thread.
    unsafe { dispatch_semaphore_signal(ring.wake) };
    0
}

// ---------------------------------------------------------------------------------- the stream

/// An output stream on the default device. See this module's header.
pub(super) struct AudioOutput {
    unit: AudioUnit,
    ring: Arc<Ring>,
    format: OutputFormat,
    period: u32,
    running: bool,
}

// SAFETY: the unit is an `AudioComponentInstance`, which Core Audio allows to be driven from any
// one thread at a time; the seam's wrapper makes `AudioOutput` `!Sync`, so it is.
unsafe impl Send for AudioOutput {}

impl AudioOutput {
    pub(super) fn open(buffer_frames: u32) -> AudioResult<Self> {
        const OP: &str = "open";
        let device = default_output_device()?;
        let period = device_period(device)?;

        let description = AudioComponentDescription {
            component_type: K_AUDIO_UNIT_TYPE_OUTPUT,
            component_sub_type: K_AUDIO_UNIT_SUB_TYPE_DEFAULT_OUTPUT,
            component_manufacturer: K_AUDIO_UNIT_MANUFACTURER_APPLE,
            component_flags: 0,
            component_flags_mask: 0,
        };
        // SAFETY: a description on the stack.
        let component = unsafe { AudioComponentFindNext(core::ptr::null_mut(), &raw const description) };
        if component.is_null() {
            return Err(AudioError::NoDevice { operation: OP });
        }
        let mut unit: AudioUnit = core::ptr::null_mut();
        // SAFETY: writes the new instance.
        os(OP, "AudioComponentInstanceNew", unsafe { AudioComponentInstanceNew(component, &raw mut unit) })?;
        // From here the unit is disposed on every failure.
        let fail = |error: AudioError| {
            // SAFETY: the instance created above, not yet handed anywhere.
            unsafe { AudioComponentInstanceDispose(unit) };
            error
        };

        let mut device_format = AudioStreamBasicDescription::default();
        let mut size = core::mem::size_of::<AudioStreamBasicDescription>() as u32;
        // SAFETY: reads the output scope's format into a struct of the size given.
        os(OP, "AudioUnitGetProperty(kAudioUnitProperty_StreamFormat, output)", unsafe {
            AudioUnitGetProperty(unit, K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT, K_AUDIO_UNIT_SCOPE_OUTPUT, 0, (&raw mut device_format).cast(), &raw mut size)
        })
        .map_err(fail)?;
        let rate = device_format.sample_rate.round();
        let channels = device_format.channels_per_frame;
        if !(rate >= 1.0 && rate <= f64::from(u32::MAX)) || channels == 0 || channels > u32::from(u16::MAX) {
            return Err(fail(AudioError::DeviceFormatUnusable {
                operation: OP,
                sample_rate: device_format.sample_rate as u64,
                channels,
            }));
        }
        let format = OutputFormat { sample_rate: rate as u32, channels: channels as u16 };

        let ours = AudioStreamBasicDescription {
            sample_rate: device_format.sample_rate,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
            bytes_per_packet: 4 * channels,
            frames_per_packet: 1,
            bytes_per_frame: 4 * channels,
            channels_per_frame: channels,
            bits_per_channel: 32,
            reserved: 0,
        };
        // SAFETY: sets the input scope's format from a struct of the size given.
        os(OP, "AudioUnitSetProperty(kAudioUnitProperty_StreamFormat, input)", unsafe {
            AudioUnitSetProperty(unit, K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT, K_AUDIO_UNIT_SCOPE_INPUT, 0, (&raw const ours).cast(), core::mem::size_of::<AudioStreamBasicDescription>() as u32)
        })
        .map_err(fail)?;

        let capacity = buffer_frames.max(period.saturating_mul(2)).max(1);
        // SAFETY: no arguments beyond the initial count.
        let wake = unsafe { dispatch_semaphore_create(0) };
        let ring = Arc::new(Ring {
            samples: (0..capacity as usize * channels as usize).map(|_| UnsafeCell::new(0.0)).collect(),
            capacity: u64::from(capacity),
            channels: channels as usize,
            written: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            wake,
        });
        let callback = AuRenderCallbackStruct { input_proc: render, input_proc_ref_con: Arc::as_ptr(&ring).cast_mut().cast() };
        // SAFETY: the callback's context is the ring, which `self` keeps alive past the unit.
        os(OP, "AudioUnitSetProperty(kAudioUnitProperty_SetRenderCallback)", unsafe {
            AudioUnitSetProperty(unit, K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK, K_AUDIO_UNIT_SCOPE_INPUT, 0, (&raw const callback).cast(), core::mem::size_of::<AuRenderCallbackStruct>() as u32)
        })
        .map_err(fail)?;
        // SAFETY: a configured unit.
        os(OP, "AudioUnitInitialize", unsafe { AudioUnitInitialize(unit) }).map_err(fail)?;
        Ok(AudioOutput { unit, ring, format, period, running: false })
    }

    pub(super) fn format(&self) -> OutputFormat {
        self.format
    }

    pub(super) fn buffer_frames(&self) -> u32 {
        self.ring.capacity as u32
    }

    pub(super) fn period_frames(&self) -> u32 {
        self.period
    }

    /// The ring's free space. Cannot fail on this backend; the `Result` is the seam's shape.
    pub(super) fn writable_frames(&self, operation: &'static str) -> AudioResult<u32> {
        let _ = operation;
        Ok(self.ring.free())
    }

    pub(super) fn wait_writable(&self, timeout: Duration) -> AudioResult<u32> {
        let nanos = i64::try_from(timeout.as_nanos()).unwrap_or(i64::MAX);
        // SAFETY: libdispatch calls on the ring's live semaphore.
        unsafe {
            let deadline = dispatch_time(DISPATCH_TIME_NOW, nanos);
            if dispatch_semaphore_wait(self.ring.wake, deadline) == 0 {
                // Collapse the periods that passed unwatched into this one wake.
                while dispatch_semaphore_wait(self.ring.wake, DISPATCH_TIME_NOW) == 0 {}
            }
        }
        Ok(self.ring.free())
    }

    /// Copy into the ring. The seam has checked that `frames` fit and that `samples` is
    /// `frames` whole frames.
    pub(super) fn write(&mut self, samples: &[f32], frames: u32) -> AudioResult<()> {
        self.ring.push(samples, frames);
        Ok(())
    }

    pub(super) fn start(&mut self) -> AudioResult<()> {
        if !self.running {
            // SAFETY: an initialized unit.
            os("start", "AudioOutputUnitStart", unsafe { AudioOutputUnitStart(self.unit) })?;
            self.running = true;
        }
        Ok(())
    }

    /// `AudioOutputUnitStop`, which returns once the I/O thread has stopped calling back, so what
    /// is still in the ring stays there.
    pub(super) fn stop(&mut self) -> AudioResult<()> {
        if self.running {
            // SAFETY: an initialized unit.
            os("stop", "AudioOutputUnitStop", unsafe { AudioOutputUnitStop(self.unit) })?;
            self.running = false;
        }
        Ok(())
    }
}

impl Drop for AudioOutput {
    /// Stop, uninitialise and dispose **before** the ring goes: the callback's context is the ring.
    fn drop(&mut self) {
        // SAFETY: the unit this output created; after disposal nothing calls `render` again.
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

/// `kAudioHardwarePropertyDefaultOutputDevice`; `kAudioObjectUnknown` (0) is no device.
fn default_output_device() -> AudioResult<u32> {
    let address = AudioObjectPropertyAddress {
        selector: K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE,
        scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        element: 0,
    };
    let mut device = 0u32;
    let mut size = 4u32;
    // SAFETY: reads a `u32` into a `u32`.
    os("open", "AudioObjectGetPropertyData(kAudioHardwarePropertyDefaultOutputDevice)", unsafe {
        AudioObjectGetPropertyData(K_AUDIO_OBJECT_SYSTEM_OBJECT, &raw const address, 0, core::ptr::null(), &raw mut size, (&raw mut device).cast())
    })?;
    if device == 0 {
        return Err(AudioError::NoDevice { operation: "open" });
    }
    Ok(device)
}

/// `kAudioDevicePropertyBufferFrameSize`: the frames one I/O cycle takes.
fn device_period(device: u32) -> AudioResult<u32> {
    let address = AudioObjectPropertyAddress {
        selector: K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE,
        scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        element: 0,
    };
    let mut frames = 0u32;
    let mut size = 4u32;
    // SAFETY: reads a `u32` into a `u32`.
    os("open", "AudioObjectGetPropertyData(kAudioDevicePropertyBufferFrameSize)", unsafe {
        AudioObjectGetPropertyData(device, &raw const address, 0, core::ptr::null(), &raw mut size, (&raw mut frames).cast())
    })?;
    Ok(frames.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(capacity: u64, channels: usize) -> Ring {
        Ring {
            samples: (0..capacity as usize * channels).map(|_| UnsafeCell::new(0.0)).collect(),
            capacity,
            channels,
            written: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            // SAFETY: no arguments beyond the count.
            wake: unsafe { dispatch_semaphore_create(0) },
        }
    }

    /// Frames come out in the order they went in, across the wrap, and a shortfall is silence.
    #[test]
    fn the_ring_keeps_order_across_the_wrap_and_pads_an_underrun_with_silence() {
        let ring = ring(4, 2);
        ring.push(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0], 3);
        assert_eq!(ring.free(), 1);
        let mut out = [9.0; 4];
        assert_eq!(ring.pull(&mut out, 2), 2);
        assert_eq!(out, [1.0, -1.0, 2.0, -2.0]);
        ring.push(&[4.0, -4.0, 5.0, -5.0, 6.0, -6.0], 3); // wraps
        assert_eq!(ring.free(), 0);
        let mut out = [9.0; 10];
        assert_eq!(ring.pull(&mut out, 5), 4, "four frames were queued");
        assert_eq!(out, [3.0, -3.0, 4.0, -4.0, 5.0, -5.0, 6.0, -6.0, 0.0, 0.0]);
        assert_eq!(ring.free(), 4);
    }

    /// **MEASURED, not assumed**: the device side of the default output unit, printed, and the
    /// format the input side was set to read back. Gated like `tests/audio_live.rs`.
    #[test]
    #[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
    fn the_device_and_input_formats_are_what_open_says() {
        assert!(
            std::env::var("OMNI_AUDIO_LIVE_TESTS").is_ok_and(|v| v == "1"),
            "run with --ignored but OMNI_AUDIO_LIVE_TESTS is not 1; this opens the real device"
        );
        let output = AudioOutput::open(0).unwrap();
        let read = |scope: u32| {
            let mut format = AudioStreamBasicDescription::default();
            let mut size = core::mem::size_of::<AudioStreamBasicDescription>() as u32;
            // SAFETY: reads a format into a struct of the size given.
            let status = unsafe {
                AudioUnitGetProperty(output.unit, K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT, scope, 0, (&raw mut format).cast(), &raw mut size)
            };
            assert_eq!(status, 0);
            format
        };
        let device = read(K_AUDIO_UNIT_SCOPE_OUTPUT);
        let input = read(K_AUDIO_UNIT_SCOPE_INPUT);
        println!("device side: {device:?}\ninput side: {input:?}\nperiod {} frames", output.period_frames());
        assert_eq!(input.format_id, K_AUDIO_FORMAT_LINEAR_PCM);
        assert_eq!(input.format_flags & K_AUDIO_FORMAT_FLAG_IS_FLOAT, K_AUDIO_FORMAT_FLAG_IS_FLOAT);
        assert_eq!(input.bits_per_channel, 32);
        assert_eq!(input.bytes_per_frame, 4 * input.channels_per_frame, "interleaved: one buffer, every channel");
        assert_eq!(input.sample_rate, device.sample_rate, "no rate conversion");
        assert_eq!(input.channels_per_frame, device.channels_per_frame, "no channel mapping");
        assert_eq!(u32::from(output.format().channels), device.channels_per_frame);
    }

    /// Core Audio's failures are mostly four-character codes; the message carries both spellings.
    #[test]
    fn an_osstatus_prints_as_its_four_characters_when_it_is_one() {
        let coded = AudioError::OsStatus { operation: "open", api: "AudioUnitInitialize", status: i32::from_be_bytes(*b"!fmt") };
        assert!(coded.to_string().contains("OSStatus 560360820 ('!fmt')"), "{coded}");
        let plain = AudioError::OsStatus { operation: "open", api: "AudioUnitInitialize", status: -50 };
        assert!(plain.to_string().contains("OSStatus -50 (-)"), "{plain}");
        assert!(!coded.is_unsupported());
    }

    #[test]
    fn a_four_character_code_is_big_endian() {
        assert_eq!(fourcc(b"lpcm"), 0x6C70_636D);
        assert_eq!(K_AUDIO_UNIT_SUB_TYPE_DEFAULT_OUTPUT, 0x6465_6620);
    }
}
