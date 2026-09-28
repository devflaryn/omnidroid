//! **Android's input channels, as the kernel sees them pass**: the two things the embedding needs
//! to know about the app's input that only the channel says -- whether the app holds the **pointer
//! capture**, and how long an input event took to be **handled**.
//!
//! An `InputChannel` is a `socketpair(AF_UNIX, SOCK_SEQPACKET)` between the input dispatcher (in
//! system_server) and a window of an app: one `InputMessage` a packet (AOSP 15,
//! `include/input/InputTransport.h`). The kernel carries those packets (`crate::socket`) and here
//! only reads them; nothing is changed, delayed or answered.
//!
//! # The pointer capture
//!
//! An app asks for the pointer capture (`View.requestPointerCapture`, what Roblox does for its
//! camera lock) of the dispatcher, which grants it to the focused window by sending that window a
//! `CAPTURE` message with `pointerCaptureEnabled` set -- and a `CAPTURE` with it clear when the app
//! releases it or its window loses the focus. That message is the guest's own statement of the
//! state, and the only place it is stated outside system_server: [`capture`] answers the latest
//! one. The live window (`crate::display_window`) holds the host's mouse exactly while it is set.
//!
//! # The latency of input
//!
//! The dispatcher sends each key and motion with its event time (the device's -- the evdev
//! timestamp, or an injected event's own), and the app answers each with a `FINISHED` message
//! carrying the same sequence number once its handlers have run. The time from the event to that
//! answer is the input's whole latency as the user feels it, from the host's event to the app
//! having acted on it, less only the drawing. [`report`] says it every [`REPORT_EVERY`] while
//! there is input: how many events, and the median, 90th percentile and worst latency.
//!
//! # The layout (AOSP 15, `struct InputMessage`)
//!
//! A header of `type: u32`, `seq: u32`; then the body. `KEY` is 0, `MOTION` 1, `FINISHED` 2,
//! `CAPTURE` 4. A key's and a motion's `eventTime` (`nsecs_t`) is at byte 16 of the message, a
//! motion's `action` at 68; `FINISHED` is 24 bytes (`handled` at 8, `consumeTime` at 16);
//! `CAPTURE` 16 (`eventId` at 8, `pointerCaptureEnabled` at 12). A message that is not one of
//! these, or not their size, is not read.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const KEY: u32 = 0;
const MOTION: u32 = 1;
const FINISHED: u32 = 2;
const CAPTURE: u32 = 4;
/// A motion message's size up to its pointers (header 8 + body 160).
const MOTION_HEAD: usize = 168;
/// How many dispatched events a channel remembers while it waits for their answers.
const REMEMBER: usize = 256;
/// How often [`report`] speaks while there is input.
pub const REPORT_EVERY: Duration = Duration::from_secs(5);

/// What one channel has in flight: (sequence number, event time in ns, what it was).
#[derive(Default)]
pub struct Channel {
    sent: VecDeque<(u32, i64, &'static str)>,
}

/// The capture state: 0 never said, 1 released, 2 held; the low bits. Bumped by 4 at each message,
/// so a watcher sees a release and a re-take between two looks.
static CAPTURE_STATE: AtomicU64 = AtomicU64::new(0);

/// The latest `CAPTURE` message: `None` if none was sent yet, else whether the capture is held;
/// and a counter that changes with every such message.
#[must_use]
pub fn capture() -> (Option<bool>, u64) {
    let v = CAPTURE_STATE.load(Ordering::Acquire);
    let state = match v & 3 {
        0 => None,
        1 => Some(false),
        _ => Some(true),
    };
    (state, v >> 2)
}

fn word(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4"))
}

fn long(b: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(b[at..at + 8].try_into().expect("8"))
}

/// One packet sent on a message-kind socket pair: looked at, never changed.
pub fn observe(channel: &Mutex<Channel>, bytes: &[u8]) {
    if bytes.len() < 16 {
        return;
    }
    match word(bytes, 0) {
        CAPTURE if bytes.len() == 16 && bytes[13..16] == [0, 0, 0] && bytes[12] <= 1 => {
            let held = bytes[12] == 1;
            let old = CAPTURE_STATE.load(Ordering::Acquire);
            CAPTURE_STATE.store((((old >> 2) + 1) << 2) | if held { 2 } else { 1 }, Ordering::Release);
            eprintln!("[input] the app {} the pointer capture", if held { "holds" } else { "released" });
        }
        FINISHED if bytes.len() == 24 => {
            let seq = word(bytes, 4);
            let consumed = long(bytes, 16);
            let now = crate::sys::monotonic().as_nanos() as i64;
            let found = {
                let mut c = channel.lock();
                let at = c.sent.iter().position(|&(s, ..)| s == seq);
                at.and_then(|i| c.sent.remove(i))
            };
            if let Some((_, when, kind)) = found {
                record(kind, now - when, consumed - when);
            }
        }
        MOTION if bytes.len() > MOTION_HEAD && (1..=16).contains(&word(bytes, 12)) => {
            let action = word(bytes, 68) & 0xff;
            let kind = match action {
                0 => "down",
                1 => "up",
                2 => "move",
                7 => "hover",
                8 => "scroll",
                11 | 12 => "button",
                _ => "motion",
            };
            remember(channel, word(bytes, 4), long(bytes, 16), kind);
        }
        KEY if (80..=128).contains(&bytes.len()) => remember(channel, word(bytes, 4), long(bytes, 16), "key"),
        _ => {}
    }
}

fn remember(channel: &Mutex<Channel>, seq: u32, when: i64, kind: &'static str) {
    let mut c = channel.lock();
    if c.sent.len() >= REMEMBER {
        c.sent.pop_front();
    }
    c.sent.push_back((seq, when, kind));
}

/// Latencies since the last report: (kind, event to answer, event to consumed), in ns.
struct Stats {
    since: Instant,
    seen: Vec<(&'static str, i64, i64)>,
}

static STATS: Mutex<Option<Stats>> = Mutex::new(None);
/// Every latency ever recorded, for a test to read.
static FINISHED_TOTAL: AtomicU64 = AtomicU64::new(0);

fn record(kind: &'static str, answered: i64, consumed: i64) {
    FINISHED_TOTAL.fetch_add(1, Ordering::Relaxed);
    let mut s = STATS.lock();
    let stats = s.get_or_insert_with(|| Stats { since: Instant::now(), seen: Vec::new() });
    stats.seen.push((kind, answered, consumed));
    if stats.since.elapsed() >= REPORT_EVERY {
        let seen = std::mem::take(&mut stats.seen);
        stats.since = Instant::now();
        drop(s);
        eprintln!("{}", report(&seen));
    }
}

/// How many events have been answered in this host process.
#[must_use]
pub fn finished_total() -> u64 {
    FINISHED_TOTAL.load(Ordering::Relaxed)
}

fn ms(ns: i64) -> f64 {
    ns as f64 / 1e6
}

/// The `[input]` line for a window's latencies.
fn report(seen: &[(&'static str, i64, i64)]) -> String {
    let mut all: Vec<i64> = seen.iter().map(|s| s.1).collect();
    all.sort_unstable();
    let pick = |v: &[i64], q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    let mut clicks: Vec<i64> = seen.iter().filter(|s| matches!(s.0, "down" | "up" | "button" | "key")).map(|s| s.1).collect();
    clicks.sort_unstable();
    let mut line = format!(
        "[input] {} events answered by the app in {}s: event -> handled p50 {:.1} ms, p90 {:.1}, max {:.1}",
        all.len(),
        REPORT_EVERY.as_secs(),
        ms(pick(&all, 0.5)),
        ms(pick(&all, 0.9)),
        ms(*all.last().expect("non-empty"))
    );
    if !clicks.is_empty() {
        line.push_str(&format!("; presses and keys ({}) p50 {:.1} ms, max {:.1}", clicks.len(), ms(pick(&clicks, 0.5)), ms(*clicks.last().expect("non-empty"))));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn motion(seq: u32, when: i64, action: u32) -> Vec<u8> {
        let mut m = vec![0u8; MOTION_HEAD + 80];
        m[0..4].copy_from_slice(&MOTION.to_le_bytes());
        m[4..8].copy_from_slice(&seq.to_le_bytes());
        m[12..16].copy_from_slice(&1u32.to_le_bytes());
        m[16..24].copy_from_slice(&when.to_le_bytes());
        m[68..72].copy_from_slice(&action.to_le_bytes());
        m
    }

    fn finished(seq: u32, consumed: i64) -> Vec<u8> {
        let mut m = vec![0u8; 24];
        m[0..4].copy_from_slice(&FINISHED.to_le_bytes());
        m[4..8].copy_from_slice(&seq.to_le_bytes());
        m[8] = 1;
        m[16..24].copy_from_slice(&consumed.to_le_bytes());
        m
    }

    #[test]
    fn a_capture_message_is_the_capture_state_and_anything_else_is_not() {
        let channel = Mutex::new(Channel::default());
        let before = capture().1;
        let mut m = vec![0u8; 16];
        m[0..4].copy_from_slice(&CAPTURE.to_le_bytes());
        m[12] = 1;
        observe(&channel, &m);
        assert_eq!(capture(), (Some(true), before + 1));
        m[12] = 0;
        observe(&channel, &m);
        assert_eq!(capture(), (Some(false), before + 2));
        m[12] = 7;
        observe(&channel, &m);
        let mut focus = m.clone();
        focus[0..4].copy_from_slice(&3u32.to_le_bytes());
        focus[12] = 1;
        observe(&channel, &focus);
        assert_eq!(capture(), (Some(false), before + 2), "neither a bad flag nor a FOCUS message is a capture");
    }

    #[test]
    fn a_motion_is_timed_from_its_event_to_its_answer() {
        let channel = Mutex::new(Channel::default());
        let now = crate::sys::monotonic().as_nanos() as i64;
        let before = finished_total();
        observe(&channel, &motion(41, now - 30_000_000, 0));
        observe(&channel, &motion(42, now - 20_000_000, 7));
        observe(&channel, &finished(42, now - 10_000_000));
        assert_eq!(channel.lock().sent.len(), 1, "the answered one is let go");
        observe(&channel, &finished(99, now));
        assert!(finished_total() > before);
        let line = report(&[("hover", 20_000_000, 10_000_000), ("down", 40_000_000, 1)]);
        assert!(line.contains("2 events") && line.contains("p50 40.0") && line.contains("presses and keys (1) p50 40.0"), "{line}");
    }
}
