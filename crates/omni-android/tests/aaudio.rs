//! `libaaudio.so` driven the way FMOD drives it: `dlopen`, `dlsym`, a builder, a stream, and a data
//! callback that is **guest code** running on a guest thread the library started.
//!
//! The host side is a recording device -- the seam's test double -- so what reached "the speaker"
//! can be read back exactly. The one thing these tests exist for is the path no unit test can
//! reach: `requestStart` creating a guest thread through the guest's own `pthread_create`, that
//! thread calling the guest's callback into a guest buffer, and the samples arriving at the host.
//!
//! A second double, [`PacedDevice`], drains in real time and wakes a waiter the way the seam's
//! contract says -- once the frames it asked for are free, and at once if they already are, as
//! ALSA's level-triggered poll does. It is what the feeding loop's pacing is measured against:
//! with FMOD's buffer size far below the capacity, a loop that waited for "a period" instead of the
//! room a burst needs spun a whole core on Linux, and here spins the double's wait count up by
//! orders of magnitude.

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

mod harness;

use std::sync::Arc;
use std::time::{Duration, Instant};

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::aaudio::{
    consts, AAudio, OutputDevice, OutputSink, PlatformOutput, ENTRY_POINT, EXPORTS, SONAMES,
};
use omni_android::bionic::{Bionic, ThreadHost};
use omni_android::{AbiError, Boundary};
use omni_cpu::{ExitReason, GuestAddr};
use parking_lot::Mutex;

/// The recording device's shape: what a 48 kHz stereo host with a 10 ms period looks like.
const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const PERIOD: u32 = 480;
const BUFFER: u32 = 4 * PERIOD;

/// The data region's layout: results at 0, the call program's operands at `ARGS`, the callback's
/// counter and threshold at `COUNTER`/`STOP_AFTER`, strings from `STRINGS_AT`.
const ARGS: u32 = 0x40;
const COUNTER: u32 = 0x100;
const STOP_AFTER: u32 = 0x108;
const OUT_SLOT: u32 = 0x200;
const STRINGS_AT: usize = 0x800;

/// The sample value the guest callback writes: `0.5f32`.
const HALF: u32 = 0x3F00_0000;

/// What the host device was given.
#[derive(Default)]
struct Record {
    opens: Vec<u32>,
    samples: Vec<f32>,
    started: bool,
    stops: u32,
}

/// A device whose streams record every sample, and "play" one period per wait once started.
struct RecordingDevice(Arc<Mutex<Record>>);

impl OutputDevice for RecordingDevice {
    fn open(&self, buffer_frames: u32) -> Result<Box<dyn OutputSink>, String> {
        self.0.lock().opens.push(buffer_frames);
        Ok(Box::new(RecordingSink { record: Arc::clone(&self.0), held: 0 }))
    }
}

struct RecordingSink {
    record: Arc<Mutex<Record>>,
    held: u32,
}

impl OutputSink for RecordingSink {
    fn sample_rate(&self) -> u32 {
        RATE
    }
    fn channels(&self) -> u16 {
        CHANNELS
    }
    fn buffer_frames(&self) -> u32 {
        BUFFER
    }
    fn period_frames(&self) -> u32 {
        PERIOD
    }
    fn writable_frames(&self) -> Result<u32, String> {
        Ok(BUFFER - self.held)
    }
    fn wait_writable(&self, _frames: u32, _timeout: Duration) -> Result<u32, String> {
        std::thread::sleep(Duration::from_millis(1));
        Ok(BUFFER - self.held)
    }
    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        let frames = u32::try_from(samples.len()).unwrap() / u32::from(CHANNELS);
        if frames > BUFFER - self.held {
            return Err(format!("{frames} frames written into {} free", BUFFER - self.held));
        }
        self.held += frames;
        self.record.lock().samples.extend_from_slice(samples);
        // A started device plays a period for every period written after the first buffer, which
        // is enough to keep the feeding loop turning.
        if self.record.lock().started {
            self.held = self.held.saturating_sub(PERIOD);
        }
        Ok(())
    }
    fn start(&mut self) -> Result<(), String> {
        self.record.lock().started = true;
        self.held = self.held.saturating_sub(PERIOD);
        Ok(())
    }
    fn stop(&mut self) -> Result<(), String> {
        let mut record = self.record.lock();
        record.started = false;
        record.stops += 1;
        Ok(())
    }
}

/// FMOD's stream on the Linux host, MEASURED (`openStream ... burst 480, capacity 9120 (asked
/// 9120)`, then `setBufferSizeInFrames` leaving `size 1440/9120`): the paced device's buffer, and
/// the buffer size the tests set.
const FMOD_CAPACITY: u32 = 9_120;
const FMOD_SIZE: u32 = 1_440;

/// What the paced device has seen.
#[derive(Default)]
struct Paced {
    /// Frames queued, drained at `RATE` while `running`.
    held: f64,
    running: bool,
    /// When `held` was last drained.
    since: Option<Instant>,
    starts: u32,
    /// Every `wait_writable`, and those that came back short of what was asked for before their
    /// timeout (which the contract forbids).
    waits: u64,
    short_waits: u64,
    /// The most frames any wait asked for, and the most the host ever held after a write.
    most_asked: u32,
    most_held: f64,
    /// The device ran dry while running.
    underruns: u32,
}

impl Paced {
    /// Drain what has played since the last look.
    fn settle(&mut self) {
        let now = Instant::now();
        if self.running {
            let since = self.since.unwrap_or(now);
            let played = now.duration_since(since).as_secs_f64() * f64::from(RATE);
            if played >= self.held && self.held > 0.0 {
                self.underruns += 1;
            }
            self.held = (self.held - played).max(0.0);
        }
        self.since = Some(now);
    }

    fn free(&self) -> u32 {
        FMOD_CAPACITY - (self.held.ceil() as u32).min(FMOD_CAPACITY)
    }
}

/// A device that plays in real time at `RATE` from a `FMOD_CAPACITY`-frame buffer, with the
/// seam's waiting contract: a started stream's wait returns once `max(frames, PERIOD)` frames are
/// free -- at once if they already are, as ALSA's poll does -- and an unstarted one's runs to its
/// timeout.
struct PacedDevice(Arc<Mutex<Paced>>);

impl OutputDevice for PacedDevice {
    fn open(&self, _buffer_frames: u32) -> Result<Box<dyn OutputSink>, String> {
        Ok(Box::new(PacedSink(Arc::clone(&self.0))))
    }
}

struct PacedSink(Arc<Mutex<Paced>>);

impl OutputSink for PacedSink {
    fn sample_rate(&self) -> u32 {
        RATE
    }
    fn channels(&self) -> u16 {
        CHANNELS
    }
    fn buffer_frames(&self) -> u32 {
        FMOD_CAPACITY
    }
    fn period_frames(&self) -> u32 {
        PERIOD
    }
    fn writable_frames(&self) -> Result<u32, String> {
        let mut paced = self.0.lock();
        paced.settle();
        Ok(paced.free())
    }
    fn wait_writable(&self, frames: u32, timeout: Duration) -> Result<u32, String> {
        let deadline = Instant::now() + timeout;
        let wanted = frames.max(PERIOD);
        {
            let mut paced = self.0.lock();
            paced.waits += 1;
            paced.most_asked = paced.most_asked.max(frames);
            if !paced.running {
                drop(paced);
                std::thread::sleep(timeout);
                return self.writable_frames();
            }
        }
        loop {
            let (free, running) = {
                let mut paced = self.0.lock();
                paced.settle();
                (paced.free(), paced.running)
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if free >= wanted || left.is_zero() || !running {
                if free < wanted && !left.is_zero() {
                    self.0.lock().short_waits += 1;
                }
                return Ok(free);
            }
            let missing = f64::from(wanted - free) / f64::from(RATE);
            std::thread::sleep(Duration::from_secs_f64(missing).min(left));
        }
    }
    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        let frames = u32::try_from(samples.len()).unwrap() / u32::from(CHANNELS);
        let mut paced = self.0.lock();
        paced.settle();
        if frames > paced.free() {
            return Err(format!("{frames} frames written into {} free", paced.free()));
        }
        paced.held += f64::from(frames);
        paced.most_held = paced.most_held.max(paced.held);
        Ok(())
    }
    fn start(&mut self) -> Result<(), String> {
        let mut paced = self.0.lock();
        paced.settle();
        paced.running = true;
        paced.starts += 1;
        Ok(())
    }
    fn stop(&mut self) -> Result<(), String> {
        let mut paced = self.0.lock();
        paced.settle();
        paced.running = false;
        Ok(())
    }
}

/// A guest with bionic (for `dlopen`, `dlsym`, `pthread_create`), a thread host that carries the
/// AAudio instance, and -- unless the test is about its absence -- a bound AAudio.
struct Fixture {
    guest: Guest,
    bionic: Arc<Bionic>,
    audio: Option<Arc<AAudio>>,
    record: Arc<Mutex<Record>>,
    boundary: Arc<Boundary>,
    call: GuestAddr,
    callback: GuestAddr,
    next_string: std::cell::Cell<usize>,
    _root: Scratch,
}

struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(tag: &str, with_audio: bool) -> Fixture {
    let record = Arc::new(Mutex::new(Record::default()));
    let device = with_audio
        .then(|| Arc::new(RecordingDevice(Arc::clone(&record))) as Arc<dyn OutputDevice>);
    fixture_on(tag, device, record, HALF)
}

/// [`fixture`] over the paced device.
fn paced_fixture(tag: &str) -> (Fixture, Arc<Mutex<Paced>>) {
    let paced = Arc::new(Mutex::new(Paced::default()));
    let device = Arc::new(PacedDevice(Arc::clone(&paced))) as Arc<dyn OutputDevice>;
    (fixture_on(tag, Some(device), Arc::new(Mutex::new(Record::default())), HALF), paced)
}

/// `sample` is the bits of the `f32` the guest callback writes: [`HALF`], except on a real device,
/// which is only ever given silence.
fn fixture_on(
    tag: &str,
    device: Option<Arc<dyn OutputDevice>>,
    record: Arc<Mutex<Record>>,
    sample: u32,
) -> Fixture {
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(1024);
    bionic.bind_into(&builder).expect("bind every bionic handler");
    bionic.set_log_to_stderr(false);
    let audio = device.map(|device| {
        let audio = AAudio::new(device);
        audio.bind_into(&builder).expect("bind libaaudio.so");
        audio
    });
    let mut host = ThreadHost::new(Arc::clone(&guest.backend) as Arc<dyn omni_cpu::GuestCpuBackend>)
        .with_limit(4);
    if let Some(audio) = &audio {
        host = host.with_instance(audio.thread_instance());
    }
    bionic.set_thread_host(host).expect("a thread host");
    let mut root = std::env::temp_dir();
    root.push(format!("omni-aaudio-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("a scratch directory");
    bionic.set_filesystem_root(&root).expect("a filesystem root");
    let boundary = builder.finish();

    // Every program is assembled now, before any guest thread exists (`Guest::load`'s rule).
    //
    // The call program: `x0 = (*ARGS)(ARGS[1], ARGS[2], ARGS[3], ARGS[4])`, stored at data + 0.
    let call = {
        let entry = guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(22, guest.data as u64);
        asm.push(ldr_imm(9, 22, ARGS));
        asm.push(ldr_imm(0, 22, ARGS + 8));
        asm.push(ldr_imm(1, 22, ARGS + 16));
        asm.push(ldr_imm(2, 22, ARGS + 24));
        asm.push(ldr_imm(3, 22, ARGS + 32));
        asm.push(blr(9));
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        guest.load(asm.words())
    };
    // The data callback, `int (*)(AAudioStream *, void *user, void *audio, int32_t frames)`: fill
    // `frames` stereo frames with 0.5, count the call, and answer STOP once the count reaches the
    // threshold in the data region.
    let callback = {
        let entry = guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.mov(12, u64::from(sample));
        asm.push(mov_reg(13, 3)); // frames left
        // loop: two samples per frame
        asm.push(str_w(12, 2, 0));
        asm.push(str_w(12, 2, 4));
        asm.push(add_imm(2, 2, 8));
        asm.push(subs_imm(13, 13, 1));
        asm.push(b_cond(1, -4)); // B.NE loop
        asm.mov(14, guest.data as u64);
        asm.push(ldr_imm(10, 14, COUNTER));
        asm.push(add_imm(10, 10, 1));
        asm.push(str_imm(10, 14, COUNTER));
        asm.push(ldr_imm(11, 14, STOP_AFTER));
        asm.push(sub_reg(11, 10, 11));
        asm.push(subs_imm(31, 11, 0)); // CMP x11, #0
        asm.push(b_cond(11, 3)); // B.LT continue
        asm.push(movz(0, consts::CALLBACK_RESULT_STOP as u16, 0));
        asm.push(ret(30));
        asm.push(movz(0, 0, 0)); // continue: CALLBACK_RESULT_CONTINUE
        asm.push(ret(30));
        guest.load(asm.words())
    };
    Fixture {
        guest,
        bionic,
        audio,
        record,
        boundary,
        call,
        callback,
        next_string: std::cell::Cell::new(STRINGS_AT),
        _root: Scratch(root),
    }
}

impl Fixture {
    fn thunk(&self, symbol: &str) -> GuestAddr {
        self.boundary.slot_named(symbol).unwrap_or_else(|| panic!("`{symbol}` is not bound")).address
    }

    fn cstr(&self, text: &str) -> u64 {
        let offset = self.next_string.get();
        let at = self.guest.data + offset;
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        self.guest.write_bytes(at, &bytes);
        self.next_string.set(offset + bytes.len() + 8);
        at as u64
    }

    /// Call `target` with up to four arguments through the guest, on this thread.
    fn call(&self, target: u64, args: &[u64]) -> Result<u64, AbiError> {
        self.guest.write_u64(self.guest.data + ARGS as usize, target);
        for index in 0..4 {
            let value = args.get(index).copied().unwrap_or(0);
            self.guest.write_u64(self.guest.data + ARGS as usize + 8 * (index + 1), value);
        }
        let _bionic = self.bionic.activate().expect("publish the bionic instance");
        let _audio = self.audio.as_ref().map(|audio| audio.activate());
        let mut cpu = self.guest.thread(&self.boundary);
        let exit = self.boundary.run(&mut cpu, self.call, BUDGET)?;
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        Ok(self.guest.read_u64(self.guest.data))
    }

    fn call_ok(&self, target: u64, args: &[u64]) -> u64 {
        self.call(target, args).unwrap_or_else(|error| panic!("the call refused: {error}"))
    }

    fn dlopen(&self, name: &str) -> u64 {
        let at = self.cstr(name);
        self.call_ok(self.thunk("dlopen") as u64, &[at, 2])
    }

    fn dlsym(&self, handle: u64, name: &str) -> u64 {
        let at = self.cstr(name);
        self.call_ok(self.thunk("dlsym") as u64, &[handle, at])
    }

    /// Every export, fetched the way FMOD fetches them.
    fn library(&self) -> std::collections::BTreeMap<&'static str, u64> {
        let handle = self.dlopen(SONAMES[0]);
        assert_ne!(handle, 0, "`{}` opens once an AAudio instance is bound", SONAMES[0]);
        EXPORTS
            .iter()
            .map(|name| {
                let at = self.dlsym(handle, name);
                assert_ne!(at, 0, "dlsym answers for `{name}`");
                (*name, at)
            })
            .collect()
    }

    /// FMOD's output stream: builder, low latency, game usage, output, the data callback.
    fn open_output(&self, api: &std::collections::BTreeMap<&'static str, u64>) -> u64 {
        self.open_output_asking(api, 0)
    }

    /// [`open_output`](Self::open_output), asking for a buffer capacity of `capacity` frames
    /// (`setBufferCapacityInFrames`) unless it is 0.
    fn open_output_asking(&self, api: &std::collections::BTreeMap<&'static str, u64>, capacity: u64) -> u64 {
        let out = (self.guest.data + OUT_SLOT as usize) as u64;
        assert_eq!(self.call_ok(api["AAudio_createStreamBuilder"], &[out]) as i32, consts::OK);
        let builder = self.guest.read_u64(out as usize);
        if capacity != 0 {
            self.call_ok(api["AAudioStreamBuilder_setBufferCapacityInFrames"], &[builder, capacity]);
        }
        self.call_ok(api["AAudioStreamBuilder_setPerformanceMode"], &[builder, 12]);
        self.call_ok(api["AAudioStreamBuilder_setUsage"], &[builder, 14]);
        self.call_ok(api["AAudioStreamBuilder_setDirection"], &[builder, consts::DIRECTION_OUTPUT as u64]);
        self.call_ok(api["AAudioStreamBuilder_setDataCallback"], &[builder, self.callback as u64, 0x1234]);
        assert_eq!(self.call_ok(api["AAudioStreamBuilder_openStream"], &[builder, out]) as i32, consts::OK);
        let stream = self.guest.read_u64(out as usize);
        assert_eq!(self.call_ok(api["AAudioStreamBuilder_delete"], &[builder]) as i32, consts::OK);
        stream
    }

    fn state_of(&self, api: &std::collections::BTreeMap<&'static str, u64>, stream: u64) -> i32 {
        self.call_ok(api["AAudioStream_getState"], &[stream]) as i32
    }

    fn wait_for_state(&self, api: &std::collections::BTreeMap<&'static str, u64>, stream: u64, want: i32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let now = self.state_of(api, stream);
            if now == want {
                return;
            }
            assert!(Instant::now() < deadline, "the stream stayed in state {now}, waiting for {want}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn finish(&self) {
        self.bionic.stop_guest_threads();
        assert!(self.bionic.join_guest_threads(Duration::from_secs(10)), "every guest thread returned");
        assert!(
            self.bionic.guest_thread_failures().is_empty(),
            "no guest thread was killed: {:?}",
            self.bionic.guest_thread_failures()
        );
    }
}

/// **The whole of FMOD's path.** The stream is the host's shape, `requestStart` starts a guest
/// thread that calls the guest's callback burst by burst, what the callback wrote is what the host
/// device received, and the callback's STOP stops the stream.
#[test]
fn a_started_stream_feeds_the_host_from_a_guest_callback_on_a_guest_thread() {
    let _serial = serialized();
    let f = fixture("feed", true);
    let api = f.library();
    let stream = f.open_output(&api);

    assert_eq!(f.call_ok(api["AAudioStream_getSampleRate"], &[stream]) as i32, RATE as i32);
    assert_eq!(f.call_ok(api["AAudioStream_getChannelCount"], &[stream]) as i32, i32::from(CHANNELS));
    assert_eq!(f.call_ok(api["AAudioStream_getFramesPerBurst"], &[stream]) as i32, PERIOD as i32);
    assert_eq!(f.call_ok(api["AAudioStream_getBufferCapacityInFrames"], &[stream]) as i32, BUFFER as i32);
    assert_eq!(
        f.call_ok(api["AAudioStream_getFormat"], &[stream]) as i32,
        consts::FORMAT_PCM_FLOAT,
        "no format was asked for, so it is the host mix format's float"
    );
    assert_eq!(f.state_of(&api, stream), consts::STATE_OPEN);

    // STOP on the fourth call: the first buffer's worth, before the device is started.
    f.guest.write_u64(f.guest.data + STOP_AFTER as usize, 4);
    assert_eq!(f.call_ok(api["AAudioStream_requestStart"], &[stream]) as i32, consts::OK);
    f.wait_for_state(&api, stream, consts::STATE_STOPPED);

    assert_eq!(f.guest.read_u64(f.guest.data + COUNTER as usize), 4, "the callback ran four times");
    {
        let record = f.record.lock();
        assert_eq!(record.samples.len(), (4 * PERIOD * u32::from(CHANNELS)) as usize);
        assert!(record.samples.iter().all(|&s| s == 0.5), "every sample is what the guest wrote");
    }
    assert_eq!(f.call_ok(api["AAudioStream_close"], &[stream]) as i32, consts::OK);
    f.finish();
}

/// **A stop asked for from another thread ends a running stream**, after the device was started,
/// and the stream can be closed.
#[test]
fn a_running_stream_stops_when_asked_and_closes() {
    let _serial = serialized();
    let f = fixture("stop", true);
    let api = f.library();
    let stream = f.open_output(&api);
    f.guest.write_u64(f.guest.data + STOP_AFTER as usize, u64::MAX >> 1);
    assert_eq!(f.call_ok(api["AAudioStream_requestStart"], &[stream]) as i32, consts::OK);
    f.wait_for_state(&api, stream, consts::STATE_STARTED);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.record.lock().started || f.guest.read_u64(f.guest.data + COUNTER as usize) < 8 {
        assert!(Instant::now() < deadline, "the device was never started and fed past its first buffer");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        f.call_ok(api["AAudioStream_requestStart"], &[stream]) as i32,
        consts::ERROR_INVALID_STATE,
        "a started stream cannot be started again"
    );
    assert_eq!(f.call_ok(api["AAudioStream_requestStop"], &[stream]) as i32, consts::OK);
    f.wait_for_state(&api, stream, consts::STATE_STOPPED);
    assert!(f.record.lock().stops >= 1, "the host device was stopped");
    let fed = f.record.lock().samples.len();
    assert!(fed >= (8 * PERIOD * u32::from(CHANNELS)) as usize, "{fed} samples");
    assert_eq!(f.call_ok(api["AAudioStream_close"], &[stream]) as i32, consts::OK);
    f.finish();
}

/// The callbacks the guest has run.
fn callbacks(f: &Fixture) -> u64 {
    f.guest.read_u64(f.guest.data + COUNTER as usize)
}

/// Callbacks per second and waits per callback over `window` of a running stream.
fn pace(f: &Fixture, paced: &Mutex<Paced>, window: Duration) -> (f64, f64) {
    let (calls, waits, from) = (callbacks(f), paced.lock().waits, Instant::now());
    std::thread::sleep(window);
    let (calls, waits) = (callbacks(f) - calls, paced.lock().waits - waits);
    let per_second = calls as f64 / from.elapsed().as_secs_f64();
    (per_second, waits as f64 / calls.max(1) as f64)
}

/// **FMOD's buffer size, far below the capacity, is fed a burst at a time -- without spinning.**
/// 1,440 frames of a 9,120-frame buffer, with a 480-frame burst: more than a period is always free,
/// so a feeding loop that waited for a period got an immediate answer, found no burst it was
/// allowed to ask for, and asked again -- MEASURED on Linux as one guest thread at 88-96 % of a
/// core for the whole session. Here the callbacks come at the device's rate (one burst per
/// 10 ms), the host never holds more than the buffer size, and there are about as many waits as
/// callbacks rather than orders of magnitude more.
#[test]
fn a_buffer_size_below_the_capacity_is_fed_a_burst_at_a_time_without_spinning() {
    let _serial = serialized();
    let (f, paced) = paced_fixture("paced");
    let api = f.library();
    let stream = f.open_output(&api);
    let capacity = f.call_ok(api["AAudioStream_getBufferCapacityInFrames"], &[stream]) as u32;
    assert_eq!(capacity, FMOD_CAPACITY);
    assert_eq!(
        f.call_ok(api["AAudioStream_setBufferSizeInFrames"], &[stream, u64::from(FMOD_SIZE)]) as i32,
        FMOD_SIZE as i32
    );
    f.guest.write_u64(f.guest.data + STOP_AFTER as usize, u64::MAX >> 1);
    assert_eq!(f.call_ok(api["AAudioStream_requestStart"], &[stream]) as i32, consts::OK);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !paced.lock().running || callbacks(&f) < 10 {
        assert!(Instant::now() < deadline, "the device was never started and fed");
        std::thread::sleep(Duration::from_millis(2));
    }

    let (per_second, waits_per_call) = pace(&f, &paced, Duration::from_secs(1));
    let expected = f64::from(RATE) / f64::from(PERIOD);
    println!(
        "{per_second:.1} callbacks/s (a burst every {PERIOD} frames at {RATE} Hz: {expected}); \
         {waits_per_call:.2} waits per callback; {} underrun(s)",
        paced.lock().underruns
    );
    assert!(
        (0.7 * expected..=1.3 * expected).contains(&per_second),
        "{per_second:.1} callbacks/s where the device plays {expected} bursts/s"
    );
    assert!(
        waits_per_call <= 3.0,
        "{waits_per_call:.1} waits per callback: the feeding loop is asking the device again and \
         again for room it may not yet use"
    );
    {
        let paced = paced.lock();
        assert_eq!(
            paced.most_asked,
            FMOD_CAPACITY - FMOD_SIZE + PERIOD,
            "the wait is for the room a burst needs"
        );
        assert!(paced.most_held <= f64::from(FMOD_SIZE), "the host held {} frames", paced.most_held);
        assert_eq!(paced.short_waits, 0, "the double kept its own contract");
    }

    assert_eq!(f.call_ok(api["AAudioStream_requestStop"], &[stream]) as i32, consts::OK);
    f.wait_for_state(&api, stream, consts::STATE_STOPPED);
    assert_eq!(f.call_ok(api["AAudioStream_close"], &[stream]) as i32, consts::OK);
    f.finish();
}

/// **A stream started again after a pause plays what its host kept**, and is fed on. The host
/// stream's stop keeps its queue (the seam's semantics on every backend), so the restarted thread
/// can find no room for a burst before the host plays -- and nothing plays an unstarted host. A
/// thread that started the host only after writing asked again at once, for ever, in silence.
#[test]
fn a_stream_started_again_after_a_pause_plays_what_its_host_kept() {
    let _serial = serialized();
    let (f, paced) = paced_fixture("resume");
    let api = f.library();
    let stream = f.open_output(&api);
    f.call_ok(api["AAudioStream_setBufferSizeInFrames"], &[stream, u64::from(FMOD_SIZE)]);
    f.guest.write_u64(f.guest.data + STOP_AFTER as usize, u64::MAX >> 1);
    assert_eq!(f.call_ok(api["AAudioStream_requestStart"], &[stream]) as i32, consts::OK);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !paced.lock().running || callbacks(&f) < 10 {
        assert!(Instant::now() < deadline, "the device was never started and fed");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(f.call_ok(api["AAudioStream_requestPause"], &[stream]) as i32, consts::OK);
    f.wait_for_state(&api, stream, consts::STATE_PAUSED);
    {
        // What a pause can leave queued: more than the buffer size less a burst, so no burst fits
        // until the host has played some of it.
        let mut paced = paced.lock();
        assert!(!paced.running, "the pause stopped the host");
        paced.held = f64::from(FMOD_SIZE - PERIOD / 2);
    }

    let before = callbacks(&f);
    assert_eq!(f.call_ok(api["AAudioStream_requestStart"], &[stream]) as i32, consts::OK);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !paced.lock().running || callbacks(&f) < before + 10 {
        assert!(
            Instant::now() < deadline,
            "restarted, the stream's host was {} and {} callbacks ran in 5 s",
            if paced.lock().running { "started" } else { "never started" },
            callbacks(&f) - before
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let (_, waits_per_call) = pace(&f, &paced, Duration::from_millis(500));
    assert!(waits_per_call <= 3.0, "{waits_per_call:.1} waits per callback after the restart");
    assert_eq!(paced.lock().starts, 2);

    assert_eq!(f.call_ok(api["AAudioStream_requestStop"], &[stream]) as i32, consts::OK);
    f.wait_for_state(&api, stream, consts::STATE_STOPPED);
    assert_eq!(f.call_ok(api["AAudioStream_close"], &[stream]) as i32, consts::OK);
    f.finish();
}

/// The feeding thread, as the real device's sink meets it: the first call from the data-callback
/// thread names it, so that its processor time can be read from the test's own thread, and every
/// wait is counted.
#[derive(Default)]
struct Meter {
    thread: Option<omni_platform::sampler::HostThread>,
    waits: u64,
}

/// [`PlatformOutput`], metered.
struct MeteredDevice(Arc<Mutex<Meter>>);

impl OutputDevice for MeteredDevice {
    fn open(&self, buffer_frames: u32) -> Result<Box<dyn OutputSink>, String> {
        Ok(Box::new(MeteredSink { inner: PlatformOutput.open(buffer_frames)?, meter: Arc::clone(&self.0) }))
    }
}

struct MeteredSink {
    inner: Box<dyn OutputSink>,
    meter: Arc<Mutex<Meter>>,
}

impl MeteredSink {
    fn on_feeding_thread(&self) {
        let mut meter = self.meter.lock();
        if meter.thread.is_none() {
            let thread = omni_platform::sampler::HostThread::current();
            meter.thread = Some(thread.expect("the feeding thread's CPU clock"));
        }
    }
}

impl OutputSink for MeteredSink {
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
    fn buffer_frames(&self) -> u32 {
        self.inner.buffer_frames()
    }
    fn period_frames(&self) -> u32 {
        self.inner.period_frames()
    }
    fn writable_frames(&self) -> Result<u32, String> {
        self.on_feeding_thread();
        self.inner.writable_frames()
    }
    fn wait_writable(&self, frames: u32, timeout: Duration) -> Result<u32, String> {
        self.meter.lock().waits += 1;
        self.inner.wait_writable(frames, timeout)
    }
    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        self.inner.write(samples)
    }
    fn start(&mut self) -> Result<(), String> {
        self.inner.start()
    }
    fn stop(&mut self) -> Result<(), String> {
        self.inner.stop()
    }
}

/// **FMOD's stream on the host's real default device is fed at the device's rate by a thread that
/// sleeps between bursts.** The defect as the game met it, without the game: FMOD's builder asks
/// for a 9,120-frame capacity and sets a buffer size of three bursts (MEASURED on Linux: burst 480,
/// `size 1440/9120`), so more than a device period is always free. Before the fix the callback
/// thread spun on the device's wait there -- 88-96 % of a core in the Pet Simulator 99 world. Here
/// the data-callback thread's own processor time over two seconds is the measure, through
/// `PlatformOutput` wrapped to name the thread that feeds it.
///
/// Opens the host's real playback device and writes **silence only** (the callback's samples are
/// `0.0`); `tests/audio_live.rs`'s gate -- `#[ignore]`d, and run with `--ignored` without
/// `OMNI_AUDIO_LIVE_TESTS=1` it fails rather than passing unrun.
#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn fmods_stream_on_the_host_device_is_fed_at_its_rate_without_spinning() {
    assert!(
        std::env::var("OMNI_AUDIO_LIVE_TESTS").is_ok_and(|v| v == "1"),
        "run with --ignored but OMNI_AUDIO_LIVE_TESTS is not 1; this test opens the host's real \
         playback device (silence only) and will not pretend to pass without one"
    );
    let _serial = serialized();
    let meter = Arc::new(Mutex::new(Meter::default()));
    let device = Arc::new(MeteredDevice(Arc::clone(&meter))) as Arc<dyn OutputDevice>;
    let f = fixture_on("live", Some(device), Arc::new(Mutex::new(Record::default())), 0);
    let api = f.library();
    let stream = f.open_output_asking(&api, u64::from(FMOD_CAPACITY));
    let rate = f.call_ok(api["AAudioStream_getSampleRate"], &[stream]) as u32;
    let burst = f.call_ok(api["AAudioStream_getFramesPerBurst"], &[stream]) as u32;
    let capacity = f.call_ok(api["AAudioStream_getBufferCapacityInFrames"], &[stream]) as u32;
    let size = f.call_ok(api["AAudioStream_setBufferSizeInFrames"], &[stream, u64::from(3 * burst)]) as u32;
    assert_eq!(size, 3 * burst, "capacity {capacity}");
    f.guest.write_u64(f.guest.data + STOP_AFTER as usize, u64::MAX >> 1);
    assert_eq!(f.call_ok(api["AAudioStream_requestStart"], &[stream]) as i32, consts::OK);
    let deadline = Instant::now() + Duration::from_secs(10);
    while callbacks(&f) < 20 {
        assert!(Instant::now() < deadline, "the stream was not fed 20 bursts in 10 s");
        std::thread::sleep(Duration::from_millis(5));
    }

    // One lock at a time: a guard in a tuple lives to the end of the statement.
    let reading = || {
        let meter = meter.lock();
        let cpu = meter.thread.as_ref().expect("the feeding thread was named").cpu_time().unwrap();
        (callbacks(&f), meter.waits, cpu, Instant::now())
    };
    let (calls, waits, cpu, from) = reading();
    std::thread::sleep(Duration::from_secs(2));
    let (calls_to, waits_to, cpu_to, to) = reading();
    let (calls, waits, cpu, wall) = (calls_to - calls, waits_to - waits, cpu_to - cpu, to - from);
    let per_second = calls as f64 / wall.as_secs_f64();
    let expected = f64::from(rate) / f64::from(burst);
    let load = cpu.as_secs_f64() / wall.as_secs_f64();
    println!(
        "{rate} Hz, burst {burst}, capacity {capacity}, size {size}: {per_second:.1} callbacks/s \
         (the device plays {expected:.1} bursts/s), {waits} waits for {calls} callbacks; the \
         data-callback thread used {cpu:?} in {wall:?} ({:.2} % of one processor)",
        100.0 * load
    );
    assert!(
        (0.75 * expected..=1.25 * expected).contains(&per_second),
        "{per_second:.1} callbacks/s where the device plays {expected:.1} bursts/s"
    );
    assert!(
        load < 0.10,
        "the data-callback thread used {cpu:?} of processor time in {wall:?}: it is polling"
    );
    assert!(waits <= 2 * calls + 20, "{waits} waits for {calls} callbacks");

    assert_eq!(f.call_ok(api["AAudioStream_requestStop"], &[stream]) as i32, consts::OK);
    f.wait_for_state(&api, stream, consts::STATE_STOPPED);
    assert_eq!(f.call_ok(api["AAudioStream_close"], &[stream]) as i32, consts::OK);
    f.finish();
}

/// **What is refused, is refused by name** -- an `AAudio*` name this library does not export, and a
/// stream handle it never issued -- and an input stream is AAudio's own "no device".
#[test]
fn an_unexported_name_and_a_forged_handle_are_refused_and_input_is_unavailable() {
    let _serial = serialized();
    let f = fixture("refuse", true);
    let handle = f.dlopen(SONAMES[0]);
    let at = f.cstr("AAudioStream_write");
    match f.call(f.thunk("dlsym") as u64, &[handle, at]) {
        Err(error) => assert!(error.to_string().contains("AAudioStream_write"), "{error}"),
        Ok(found) => panic!("dlsym of an unexported AAudio name answered {found:#x}"),
    }
    assert_eq!(f.dlsym(handle, "memcpy"), 0, "a non-AAudio name is an ordinary miss");

    let api = f.library();
    let out = (f.guest.data + OUT_SLOT as usize) as u64;
    f.call_ok(api["AAudio_createStreamBuilder"], &[out]);
    let builder = f.guest.read_u64(out as usize);
    f.call_ok(api["AAudioStreamBuilder_setDirection"], &[builder, consts::DIRECTION_INPUT as u64]);
    f.guest.write_u64(out as usize, 0);
    assert_eq!(
        f.call_ok(api["AAudioStreamBuilder_openStream"], &[builder, out]) as i32,
        consts::ERROR_UNAVAILABLE,
        "there is no audio input device, which is AAudio's UNAVAILABLE"
    );
    assert_eq!(f.guest.read_u64(out as usize), 0, "and no stream was handed out");
    match f.call(api["AAudioStream_getState"], &[builder]) {
        Err(error) => assert!(error.to_string().contains("not a live AAudioStream"), "{error}"),
        Ok(state) => panic!("a builder handle was answered as a stream: {state}"),
    }
    f.finish();
}

/// **Without an AAudio instance, `libaaudio.so` does not open** -- FMOD's NOSOUND fallback, which
/// is the truth for an embedding that supplies no output.
#[test]
fn without_an_instance_libaaudio_answers_null() {
    let _serial = serialized();
    let f = fixture("absent", false);
    assert_eq!(f.dlopen(SONAMES[0]), 0);
    assert!(f.boundary.slot_named(ENTRY_POINT).is_none());
    f.finish();
}
