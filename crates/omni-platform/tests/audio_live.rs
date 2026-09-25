//! Tests that open a **real** audio output, and therefore need a playback device.
//!
//! # The gate
//!
//! The same arrangement as `tests/window_live.rs`, for the same reason (VERIFICATION entry 4: a
//! test that cannot run must fail, not skip):
//!
//! * **Not asked for.** Every test here is `#[ignore]`d with a reason that names the variable, so
//!   an ordinary `cargo test` reports them as `ignored` — a line saying they did not run.
//! * **Asked for.** `--ignored` means someone decided these should run. Without
//!   `OMNI_AUDIO_LIVE_TESTS=1` they **panic** naming the variable rather than pass.
//!
//! Run them with:
//!
//! ```text
//! OMNI_AUDIO_LIVE_TESTS=1 cargo test -p omni-platform --release --test audio_live -- --ignored --nocapture
//! ```
//!
//! # Silence only
//!
//! These run on a person's desktop, on whatever their speakers are. **Every sample written is
//! `0.0`.** What is being tested is that the host takes samples and consumes them at its rate, and
//! silence proves that exactly as well as a tone does: the device's padding drains whether the
//! frames are loud or not.
//!
//! # What each assertion could catch
//!
//! Every assertion here is one a broken implementation would fail, and the controls are there to
//! make that true: a stream that is **not** started must not drain, so that the drain seen after
//! `start` is the device consuming and not something that happens to any stream; a refused write
//! must leave the free space where it was, on a stream that is not started, so that "unchanged"
//! cannot be the device draining exactly as much as was wrongly queued.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use omni_platform::audio::{AudioError, AudioOutput};
use omni_platform::sampler::HostThread;

/// The opt-in.
const GATE: &str = "OMNI_AUDIO_LIVE_TESTS";

/// Fail — loudly, naming the variable — if these were run without the opt-in.
///
/// Reaching this function at all means `--ignored` was passed, i.e. somebody asked for the audio
/// tests. Answering that request with a silent success is the defect VERIFICATION entry 4 records.
fn require_gate() {
    let set = std::env::var(GATE).is_ok_and(|v| v == "1");
    assert!(
        set,
        "this test was run with --ignored but {GATE} is not set to 1. It opens the host's real \
         default playback device (and writes only silence to it); it will not pretend to have \
         passed without one. Set {GATE}=1 to run it, or drop --ignored to skip it visibly."
    );
}

/// What the shape test asks for: 100 ms at 48 kHz. The device may give more.
const REQUEST_FRAMES: u32 = 4_800;

fn open(frames: u32) -> AudioOutput {
    match AudioOutput::open(frames) {
        Ok(output) => output,
        Err(error) => panic!(
            "AudioOutput::open({frames}) failed, on a host the gate says has a playback device: \
             {error}"
        ),
    }
}

/// `frames` frames of silence in the stream's channel count.
fn silence(output: &AudioOutput, frames: u32) -> Vec<f32> {
    vec![0.0; frames as usize * usize::from(output.format().channels)]
}

#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn the_default_device_opens_and_reports_a_usable_shape() {
    require_gate();
    let output = open(REQUEST_FRAMES);
    let format = output.format();
    println!(
        "default render device: {} Hz, {} channels; buffer {} frames for a request of \
         {REQUEST_FRAMES}; period {} frames",
        format.sample_rate,
        format.channels,
        output.buffer_frames(),
        output.period_frames()
    );

    assert!(format.sample_rate > 0, "{output:?}");
    assert!(format.channels >= 1, "{output:?}");
    assert!(output.buffer_frames() > 0, "{output:?}");
    assert!(output.period_frames() > 0, "{output:?}");
    assert!(output.period_frames() <= output.buffer_frames(), "{output:?}");
    // "The device may give more": at least what was asked for, never less.
    assert!(output.buffer_frames() >= REQUEST_FRAMES, "{output:?}");
    // A fresh stream holds nothing, so all of it is writable.
    assert_eq!(output.writable_frames(), Ok(output.buffer_frames()), "{output:?}");
}

/// Fill the buffer with silence, prove an unstarted stream holds it, then start it **on another
/// thread** — the way the runtime drives it — and prove the device drains it at its own rate and
/// signals for more.
#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn a_started_stream_drains_silence_at_its_rate_and_signals_for_more() {
    require_gate();
    // Half a second at whatever rate the device runs at, so that the drain can be timed over a
    // window well inside one buffer however slow this machine is today.
    let rate = open(0).format().sample_rate;
    let mut output = open(rate / 2);
    let (buffer, period) = (output.buffer_frames(), output.period_frames());

    let free = output.writable_frames().unwrap();
    assert_eq!(free, buffer, "a fresh stream is empty");
    output.write(&silence(&output, free)).unwrap();
    let full = output.writable_frames().unwrap();
    assert_eq!(full, 0, "writing the whole free space must leave none");

    // The control: not started, so nothing drains and nothing signals. The wait runs to its
    // timeout and says so by returning what is free, which is nothing.
    let asked = Instant::now();
    let idle = output.wait_writable(0, Duration::from_millis(100)).unwrap();
    let idled = asked.elapsed();
    assert_eq!(idle, 0, "an unstarted stream consumed audio");
    assert!(
        idled >= Duration::from_millis(90),
        "an unstarted stream's wait returned after {idled:?}; it should have run to its timeout"
    );

    // Driven, and dropped, from a thread that never initialised COM: the production shape.
    let driver = std::thread::spawn(move || {
        output.start().unwrap();
        let started = Instant::now();

        // Padding drains: the free space rises above what it was straight after the write.
        let (first_free, first_at) = loop {
            let now = output.writable_frames().unwrap();
            if now > full {
                break (now, started.elapsed());
            }
            assert!(started.elapsed() < Duration::from_secs(1), "no drain 1 s after start");
            std::thread::sleep(Duration::from_millis(1));
        };

        // …and at the device's rate, timed over a window far shorter than the buffer.
        let window_start = Instant::now();
        std::thread::sleep(Duration::from_millis(200));
        let later_free = output.writable_frames().unwrap();
        let window = window_start.elapsed();
        assert!(
            later_free < buffer,
            "the buffer ran dry inside the timing window ({window:?}); the rate cannot be read"
        );
        // Checked, not wrapped: a free space that *fell* with nothing written is itself the
        // finding, and a wrapped difference would read as a very fast device (VERIFICATION
        // entry 3).
        let drained = later_free
            .checked_sub(first_free)
            .expect("the free space fell while running with nothing written");
        let drained_per_second = f64::from(drained) / window.as_secs_f64();

        // Refill, then the device's event must wake a waiter with room again. A stale signal from
        // a period that passed before the refill can wake the first wait with nothing free, so the
        // wait is repeated — but each one must return **before** its timeout, which only the event
        // can make it do.
        let refill = output.writable_frames().unwrap();
        output.write(&silence(&output, refill)).unwrap();
        let asked = Instant::now();
        let mut wakes = 0;
        let woke = loop {
            let free = output.wait_writable(0, Duration::from_secs(2)).unwrap();
            wakes += 1;
            if free > 0 {
                break free;
            }
            assert!(asked.elapsed() < Duration::from_secs(1), "no room 1 s after a refill");
        };
        let waited = asked.elapsed();
        assert!(
            waited < Duration::from_secs(1),
            "wait_writable took {waited:?}: it ran to its timeout, so the event never fired"
        );

        // Stopped, it holds what is left.
        output.stop().unwrap();
        let held = output.writable_frames().unwrap();
        assert!(held < buffer, "the buffer ran dry before stop; nothing left to hold");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(output.writable_frames().unwrap(), held, "a stopped stream kept draining");

        (first_free, first_at, drained_per_second, window, refill, woke, waited, wakes)
        // `output` drops here, on this thread.
    });
    let (first_free, first_at, drained_per_second, window, refill, woke, waited, wakes) =
        driver.join().expect("the driving thread panicked; its message is above");

    println!(
        "{rate} Hz, buffer {buffer}, period {period}: first drain {first_at:?} after start \
         ({first_free} frames free); {drained_per_second:.0} frames/s over {window:?}; refilled \
         {refill}, woke with {woke} free after {waited:?} ({wakes} wait(s))"
    );
    let ratio = drained_per_second / f64::from(rate);
    assert!(
        (0.75..=1.25).contains(&ratio),
        "the device drained {drained_per_second:.0} frames/s at a reported {rate} Hz"
    );
}

/// A write larger than the free space is refused whole, on an **unstarted** stream so that the
/// free space is a fixed number and "nothing was written" is exactly checkable.
#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn a_write_larger_than_the_free_space_is_refused_and_writes_nothing() {
    require_gate();
    let mut output = open(REQUEST_FRAMES);
    let empty = output.writable_frames().unwrap();

    let refused = output.write(&silence(&output, empty + 1));
    assert_eq!(
        refused,
        Err(AudioError::TooManyFrames { operation: "write", frames: empty + 1, writable: empty })
    );
    assert_eq!(output.writable_frames(), Ok(empty), "a refused write queued something");

    // Part full: half in, then one frame more than the rest.
    let half = empty / 2;
    output.write(&silence(&output, half)).unwrap();
    let rest = output.writable_frames().unwrap();
    assert_eq!(rest, empty - half, "a write of {half} frames queued a different amount");
    assert_eq!(
        output.write(&silence(&output, rest + 1)),
        Err(AudioError::TooManyFrames { operation: "write", frames: rest + 1, writable: rest })
    );
    assert_eq!(output.writable_frames(), Ok(rest), "a refused write queued something");

    // Exactly the rest fits, and an empty write is no write at all.
    output.write(&silence(&output, rest)).unwrap();
    output.write(&[]).unwrap();
    assert_eq!(output.writable_frames(), Ok(0));

    // Dropped while running, so that `Drop`'s stop is exercised too; a crash in it fails the
    // process, which is the verdict (VERIFICATION entry 17).
    output.start().unwrap();
}

/// **A wait for more room than a period goes on until that much is free.** A full, started stream
/// asked for four periods' room: the device wakes a waiter a period at a time, and the wait must
/// not come back after the first wake with a period free (which is what a feeder that can only
/// use four periods would then ask again for, at once). It comes back with at least four periods
/// free, long before its timeout.
#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn a_wait_for_more_than_a_period_returns_with_that_much_free() {
    require_gate();
    let rate = open(0).format().sample_rate;
    let mut output = open(rate / 2);
    let period = output.period_frames();
    let asked_for = 4 * period;
    let driver = std::thread::spawn(move || {
        let mut rounds = Vec::new();
        for _ in 0..3 {
            let free = output.writable_frames().unwrap();
            output.write(&silence(&output, free)).unwrap();
            output.start().unwrap();
            let asked = Instant::now();
            let free = output.wait_writable(asked_for, Duration::from_secs(2)).unwrap();
            rounds.push((free, asked.elapsed()));
        }
        rounds
    });
    let rounds = driver.join().expect("the driving thread panicked; its message is above");
    println!("period {period} at {rate} Hz; waits for {asked_for} frames came back with {rounds:?}");
    for (free, waited) in rounds {
        assert!(free >= asked_for, "a wait for {asked_for} came back with {free} free after {waited:?}");
        assert!(waited < Duration::from_secs(1), "a wait for {asked_for} ran to its timeout ({waited:?})");
    }
}

/// **More room than an unstarted stream has is not a reason to wait for ever, or to poll**: a wait
/// for the whole buffer on a full stream that nothing drains runs to its timeout and answers what
/// is free, which is nothing. Run on a thread with a deadline, so that a wait which never returned
/// fails this test instead of hanging the run.
#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn a_wait_for_room_an_unstarted_stream_cannot_make_runs_to_its_timeout() {
    require_gate();
    let (done, answer) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = open(REQUEST_FRAMES);
        let buffer = output.buffer_frames();
        output.write(&silence(&output, buffer)).unwrap();
        let asked = Instant::now();
        let free = output.wait_writable(buffer, Duration::from_millis(100));
        let _ = done.send((free, asked.elapsed()));
    });
    let (free, waited) = answer
        .recv_timeout(Duration::from_secs(10))
        .expect("a 100 ms wait on an unstarted stream had not returned after 10 s");
    assert_eq!(free, Ok(0), "an unstarted stream consumed audio");
    assert!(
        (Duration::from_millis(90)..Duration::from_secs(1)).contains(&waited),
        "a 100 ms wait for room that cannot come took {waited:?}"
    );
}

/// **A feeder that keeps less queued than the buffer holds is paced by the device, not spinning.**
/// AAudio's shape as FMOD sets it (MEASURED on Linux: a 1,440-frame buffer size in a 9,120-frame
/// buffer, a 480-frame burst): here a buffer size of three periods in a buffer of about a fifth of
/// a second, a burst of one period, and each wait for the room a burst needs --
/// `buffer - size + period` free. Over three seconds on its own thread the feeder writes about a
/// burst per period and uses a small fraction of one processor. A wait that answered as soon as a
/// period was free ran this loop flat out, because more than a period is always free here: one
/// guest thread at 88-96 % of a core, for a whole Linux session.
#[test]
#[ignore = "needs an audio output device: OMNI_AUDIO_LIVE_TESTS=1 cargo test -- --ignored"]
fn a_feeder_below_the_buffer_waits_for_its_burst_rather_than_spinning() {
    require_gate();
    let rate = open(0).format().sample_rate;
    let mut output = open(rate / 5);
    let (buffer, period) = (output.buffer_frames(), output.period_frames());
    let size = 3 * period;
    assert!(size + period < buffer, "buffer {buffer}, period {period}");
    let wanted = buffer - size + period;
    const RUN: Duration = Duration::from_secs(3);
    let feeder = std::thread::spawn(move || {
        output.write(&silence(&output, size)).unwrap();
        output.start().unwrap();
        let thread = HostThread::current().expect("this thread's CPU clock");
        let (cpu_from, from) = (thread.cpu_time().unwrap(), Instant::now());
        let (mut bursts, mut waits) = (0u64, 0u64);
        let burst = silence(&output, period);
        while from.elapsed() < RUN {
            let free = output.wait_writable(wanted, Duration::from_millis(50)).unwrap();
            waits += 1;
            let mut allowed = size.saturating_sub(buffer - free);
            while allowed >= period {
                output.write(&burst).unwrap();
                bursts += 1;
                allowed -= period;
            }
        }
        let (wall, cpu) = (from.elapsed(), thread.cpu_time().unwrap() - cpu_from);
        output.stop().unwrap();
        (bursts, waits, wall, cpu)
    });
    let (bursts, waits, wall, cpu) =
        feeder.join().expect("the feeding thread panicked; its message is above");
    let per_second = bursts as f64 / wall.as_secs_f64();
    let expected = f64::from(rate) / f64::from(period);
    let load = cpu.as_secs_f64() / wall.as_secs_f64();
    println!(
        "buffer {buffer}, size {size}, burst {period} at {rate} Hz: {bursts} bursts in {wall:?} \
         ({per_second:.1}/s for {expected:.1}), {waits} waits, feeder CPU {cpu:?} ({:.2} % of the wall time)",
        100.0 * load
    );
    assert!(
        (0.75 * expected..=1.25 * expected).contains(&per_second),
        "{per_second:.1} bursts/s where the device plays {expected:.1}"
    );
    assert!(load < 0.05, "the feeder used {cpu:?} of processor time in {wall:?}: it is polling");
    assert!(waits <= 2 * bursts + 20, "{waits} waits for {bursts} bursts");
}
