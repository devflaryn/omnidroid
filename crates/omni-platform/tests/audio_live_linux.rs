//! The Linux audio backend (ALSA) against a **real** playback device: what `tests/audio_live.rs`
//! proves on every target, plus what only this backend has -- real-time consumption measured over
//! seconds, the xrun path and its count, and the stop/start semantics the Windows backend has.
//!
//! # The gate
//!
//! `tests/audio_live.rs`'s, unchanged: every test is `#[ignore]`d naming the variable, and run with
//! `--ignored` but without `OMNI_AUDIO_LIVE_TESTS=1` it **fails** rather than passing unrun
//! (VERIFICATION entry 4).
//!
//! ```text
//! OMNI_AUDIO_LIVE_TESTS=1 cargo test -p omni-platform --release --test audio_live_linux -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` because each test opens the default device, and two streams at once would
//! share the server's graph and each other's timing.
//!
//! # Silence only
//!
//! Every sample written is `0.0`; the device consumes silence at exactly the rate it consumes
//! anything else. What is measured is what ALSA **reports** -- frames free, frames consumed, the
//! recovery count -- never what the speakers do.
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use omni_platform::audio::{AudioOutput, Recoveries};

const GATE: &str = "OMNI_AUDIO_LIVE_TESTS";

fn require_gate() {
    let set = std::env::var(GATE).is_ok_and(|v| v == "1");
    assert!(
        set,
        "this test was run with --ignored but {GATE} is not set to 1. It opens the host's real \
         default playback device (and writes only silence to it); it will not pretend to have \
         passed without one. Set {GATE}=1 to run it, or drop --ignored to skip it visibly."
    );
}

fn open(frames: u32) -> AudioOutput {
    AudioOutput::open(frames).unwrap_or_else(|error| {
        panic!("AudioOutput::open({frames}) failed on a host the gate says has a device: {error}")
    })
}

fn silence(output: &AudioOutput, frames: u32) -> Vec<f32> {
    vec![0.0; frames as usize * usize::from(output.format().channels)]
}

/// Write all the free space as silence; return how many frames that was.
fn top_up(output: &mut AudioOutput) -> u32 {
    let free = output.writable_frames().unwrap();
    output.write(&silence(output, free)).unwrap();
    free
}

/// How long `frames` last at the stream's rate.
fn duration_of(output: &AudioOutput, frames: u32) -> Duration {
    Duration::from_secs_f64(f64::from(frames) / f64::from(output.format().sample_rate))
}

const NO_RECOVERIES: Recoveries = Recoveries { xruns: 0, suspends: 0 };

#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn the_default_device_reports_the_format_alsa_granted() {
    require_gate();
    for asked in [0, 4_800, 24_000] {
        let output = open(asked);
        let format = output.format();
        println!(
            "open({asked}): {} Hz x{}, buffer {}, period {}",
            format.sample_rate,
            format.channels,
            output.buffer_frames(),
            output.period_frames()
        );
        assert!(format.sample_rate > 0 && format.channels > 0, "{output:?}");
        assert!(output.period_frames() > 0, "{output:?}");
        assert!(output.period_frames() * 2 <= output.buffer_frames(), "two periods at least: {output:?}");
        assert!(output.buffer_frames() >= asked, "the host gave less than asked: {output:?}");
        assert_eq!(output.writable_frames(), Ok(output.buffer_frames()), "a fresh stream is empty");
        assert_eq!(output.recoveries(), NO_RECOVERIES);
    }
}

/// The device consumes at its granted rate, measured over **two seconds** of a stream kept fed
/// the way the runtime feeds it (wait, then write what is free). The rate is consumed frames --
/// everything written minus what is still queued -- between two readings taken after the stream
/// has settled, so neither start-up latency nor the first fill is in the figure.
#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn a_fed_stream_consumes_at_its_granted_rate_over_two_seconds() {
    require_gate();
    let rate = open(0).format().sample_rate;
    let mut output = open(rate / 5); // 200 ms of buffer
    let buffer = output.buffer_frames();

    let mut written = u64::from(top_up(&mut output));
    output.start().unwrap();
    let started = Instant::now();
    // Consumed so far = written - queued, where queued = buffer - free.
    let consumed = |output: &AudioOutput, written: u64| {
        let free = output.writable_frames().unwrap();
        written - u64::from(buffer - free)
    };
    let mut waits = 0u32;
    let mut feed_until = |output: &mut AudioOutput, written: &mut u64, until: Duration| {
        while started.elapsed() < until {
            let free = output.wait_writable(Duration::from_millis(100)).unwrap();
            waits += 1;
            output.write(&silence(output, free)).unwrap();
            *written += u64::from(free);
        }
    };

    feed_until(&mut output, &mut written, Duration::from_millis(300));
    let (from, from_at) = (consumed(&output, written), started.elapsed());
    feed_until(&mut output, &mut written, Duration::from_millis(2_300));
    let (to, to_at) = (consumed(&output, written), started.elapsed());

    let window = to_at - from_at;
    let per_second = (to - from) as f64 / window.as_secs_f64();
    let ratio = per_second / f64::from(rate);
    println!(
        "granted {rate} Hz, buffer {buffer}, period {}: {} frames consumed over {window:?} = \
         {per_second:.0} frames/s ({:.4} of the granted rate); {waits} waits; recoveries {:?}",
        output.period_frames(),
        to - from,
        ratio,
        output.recoveries()
    );
    assert!(window >= Duration::from_secs(1), "the window was {window:?}");
    assert!(
        (0.98..=1.02).contains(&ratio),
        "the device consumed {per_second:.0} frames/s at a granted {rate} Hz"
    );
    assert_eq!(output.recoveries(), NO_RECOVERIES, "a stream fed through wait_writable ran dry");
}

/// `wait_writable` honours its timeout on a stream that is **not** started -- which neither
/// drains nor signals -- whether its buffer is full (ALSA's poll would block) or empty (ALSA's poll
/// would return at once). On a running one, it wakes when a period has been consumed, long before
/// its timeout, with at least a period free. (The running timeout itself, with no period free, is
/// `audio::linux::tests::a_running_wait_returns_at_its_timeout_when_no_period_has_freed`, which
/// needs a period longer than the seam asks for.)
#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn wait_writable_runs_to_its_timeout_unless_the_device_frees_a_period() {
    require_gate();
    let mut output = open(4_800);
    let timeout = Duration::from_millis(100);

    let asked = Instant::now();
    let free = output.wait_writable(timeout).unwrap();
    let empty_wait = asked.elapsed();
    assert_eq!(free, output.buffer_frames());
    assert!(
        (timeout..timeout * 3).contains(&empty_wait),
        "an unstarted, empty stream's {timeout:?} wait took {empty_wait:?}"
    );

    top_up(&mut output);
    let asked = Instant::now();
    let free = output.wait_writable(timeout).unwrap();
    let full_wait = asked.elapsed();
    assert_eq!(free, 0, "an unstarted stream consumed audio");
    assert!(
        (timeout.mul_f32(0.9)..timeout * 3).contains(&full_wait),
        "an unstarted, full stream's {timeout:?} wait took {full_wait:?}"
    );

    output.start().unwrap();
    let period = output.period_frames();
    let mut wakes = Vec::new();
    for _ in 0..5 {
        top_up(&mut output);
        let asked = Instant::now();
        let free = output.wait_writable(Duration::from_secs(2)).unwrap();
        let woke = asked.elapsed();
        wakes.push((woke, free));
        assert!(woke < Duration::from_millis(500), "a running wait took {woke:?}: no period freed");
        assert!(free >= period, "woke with {free} free, less than the period {period}");
    }
    println!(
        "unstarted waits: empty {empty_wait:?}, full {full_wait:?}; running wakes (time, free) \
         with period {period}: {wakes:?}"
    );
    assert_eq!(output.recoveries(), NO_RECOVERIES);
}

/// A running stream left unfed runs dry: ALSA reports an xrun, the backend recovers it and
/// **counts** it, the free space reads as the whole buffer (as WASAPI's padding does after an
/// underrun), and the next write starts the stream playing again. Twice -- once found by
/// `writable_frames` (`snd_pcm_avail`), once by `wait_writable` (`snd_pcm_wait`).
#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn an_xrun_is_recovered_counted_and_played_through() {
    require_gate();
    let mut output = open(4_800);
    let buffer = output.buffer_frames();
    let dry = duration_of(&output, buffer) * 4;

    top_up(&mut output);
    output.start().unwrap();
    std::thread::sleep(dry);
    assert_eq!(output.writable_frames(), Ok(buffer), "after running dry the buffer is empty");
    assert_eq!(output.recoveries(), Recoveries { xruns: 1, suspends: 0 });

    // Played through: written again, it drains again -- the stream was restarted, not left
    // prepared and silent.
    top_up(&mut output);
    let refilled = Instant::now();
    let (drained, restarted) = loop {
        let free = output.writable_frames().unwrap();
        if free > 0 {
            break (free, refilled.elapsed());
        }
        assert!(refilled.elapsed() < Duration::from_millis(500), "a recovered stream did not restart");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(output.recoveries().xruns, 1, "draining after a refill is not an xrun");

    std::thread::sleep(dry);
    let free = output.wait_writable(Duration::from_millis(100)).unwrap();
    assert_eq!(free, buffer);
    assert_eq!(output.recoveries(), Recoveries { xruns: 2, suspends: 0 });

    top_up(&mut output);
    std::thread::sleep(duration_of(&output, buffer) / 2);
    let after = output.writable_frames().unwrap();
    assert!(after > 0 && after < buffer, "the second recovery did not play through: {after} free");
    println!(
        "buffer {buffer}: two xruns recovered and counted ({:?}); {drained} frames free {:?} \
         after the first refill; {after} free half a buffer after the second",
        output.recoveries(),
        restarted
    );
}

/// `stop` keeps what is queued and stops draining it; `start` plays it from where it was -- the
/// Windows backend's `IAudioClient::Stop`/`Start`. Repeated stops and starts do nothing, and a
/// stream started with nothing queued starts when it is first written.
#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn stop_holds_the_queue_and_start_plays_it() {
    require_gate();
    let rate = open(0).format().sample_rate;
    let mut output = open(rate / 2);
    let buffer = output.buffer_frames();

    top_up(&mut output);
    output.start().unwrap();
    output.start().unwrap(); // a started stream: nothing
    std::thread::sleep(Duration::from_millis(100));
    output.stop().unwrap();
    let held = output.writable_frames().unwrap();
    assert!(held > 0 && held < buffer, "{held} free of {buffer} after 100 ms of playing");
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(output.writable_frames(), Ok(held), "a stopped stream kept draining");
    output.stop().unwrap(); // a stopped stream: nothing
    assert_eq!(output.writable_frames(), Ok(held));

    output.start().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let resumed = output.writable_frames().unwrap();
    assert!(
        resumed > held && resumed < buffer,
        "after start the held queue should drain further but not vanish: {held} -> {resumed} free \
         of {buffer}"
    );
    output.stop().unwrap();
    assert_eq!(output.recoveries(), NO_RECOVERIES);
    drop(output);

    // Started empty: accepted, nothing drains, and the first write starts it.
    let mut output = open(4_800);
    let buffer = output.buffer_frames();
    output.start().unwrap();
    assert_eq!(output.writable_frames(), Ok(buffer));
    top_up(&mut output);
    let written = Instant::now();
    loop {
        if output.writable_frames().unwrap() > 0 {
            break;
        }
        assert!(written.elapsed() < Duration::from_millis(500), "the first write did not start it");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(output.recoveries(), NO_RECOVERIES, "starting empty is not an xrun");
    println!(
        "stop held {held} free for 150 ms; start drained on to {resumed}; an empty start began \
         draining {:?} after its first write",
        written.elapsed()
    );
    // Dropped running: `snd_pcm_close` drops the stream; a crash there fails the process.
}
