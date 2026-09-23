//! `libaaudio.so` driven the way FMOD drives it: `dlopen`, `dlsym`, a builder, a stream, and a data
//! callback that is **guest code** running on a guest thread the library started.
//!
//! The host side is a recording device -- the seam's test double -- so what reached "the speaker"
//! can be read back exactly. The one thing these tests exist for is the path no unit test can
//! reach: `requestStart` creating a guest thread through the guest's own `pthread_create`, that
//! thread calling the guest's callback into a guest buffer, and the samples arriving at the host.

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

mod harness;

use std::sync::Arc;
use std::time::{Duration, Instant};

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::aaudio::{consts, AAudio, OutputDevice, OutputSink, ENTRY_POINT, EXPORTS, SONAMES};
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
    fn wait_writable(&self, _timeout: Duration) -> Result<u32, String> {
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
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(1024);
    bionic.bind_into(&builder).expect("bind every bionic handler");
    bionic.set_log_to_stderr(false);
    let record = Arc::new(Mutex::new(Record::default()));
    let audio = with_audio.then(|| {
        let audio = AAudio::new(Arc::new(RecordingDevice(Arc::clone(&record))));
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
        asm.mov(12, u64::from(HALF));
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
        let out = (self.guest.data + OUT_SLOT as usize) as u64;
        assert_eq!(self.call_ok(api["AAudio_createStreamBuilder"], &[out]) as i32, consts::OK);
        let builder = self.guest.read_u64(out as usize);
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
