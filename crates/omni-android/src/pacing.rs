//! **`OMNI_FPS_CAP=<fps>`: this layer paces the engine's presents.**
//!
//! The engine paces itself at 60 fps (its TaskScheduler's built-in 1/60 s, HANDOFF), and the
//! `FramerateCap` a player saves is applied only when the server flag
//! `GameBasicSettingsFramerateCap` is on -- it is off in the live flag document. So an instance
//! meant to idle cheaply (the owner's many-instance case) cannot ask the engine for fewer frames.
//! This asks for them from underneath: `vkQueuePresentKHR` and `eglSwapBuffers` wait, before the
//! frame is handed to the host, until at least `1/fps` has passed since the previous present's
//! turn. The render thread waiting is what a device with a slower display does to the engine: its
//! frame loop runs at the rate presents complete, and the per-frame work -- render, the frame's
//! Lua, the jobs that wait for it -- runs that often.
//!
//! **Not a correctness change.** No frame is skipped or faked: every frame the engine renders is
//! presented, later. The pacing is a deadline schedule rather than a sleep per call, so a frame
//! that took longer than the period is not delayed further, and after a stall of more than one
//! period the schedule restarts from now rather than bursting to catch up.
//!
//! | value | meaning |
//! |---|---|
//! | unset, `0`, `off` | no pacing (the default) |
//! | `<fps>` | at most that many presents a second, `1`..=`1000`, fractions allowed |

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// The schedule: the earliest the next present may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// One frame's period.
    pub period: Duration,
    /// The next present's turn, once there has been a first.
    pub next: Option<Instant>,
}

impl Schedule {
    /// A schedule for `fps` presents a second.
    ///
    /// # Panics
    ///
    /// If `fps` is not in `1.0..=1000.0` -- [`parse_cap`] is what refuses a bad value by name.
    #[must_use]
    pub fn new(fps: f64) -> Self {
        assert!((1.0..=1000.0).contains(&fps), "an fps cap of {fps}");
        Self { period: Duration::from_secs_f64(1.0 / fps), next: None }
    }

    /// A present arriving at `now`: how long it waits, and the schedule after it.
    ///
    /// * The first present waits for nothing.
    /// * One that arrives before its turn waits until then; the next turn is one period after.
    /// * One that arrives after its turn goes now; the next turn is one period after the missed
    ///   one, so a frame that ran a little long is made up by the next -- unless it is more than a
    ///   period late, when the schedule restarts from now (no burst after a stall).
    #[must_use]
    pub fn arrive(&self, now: Instant) -> (Duration, Schedule) {
        let turn = match self.next {
            None => now,
            Some(next) if now < next => next,
            Some(next) if now.duration_since(next) <= self.period => next,
            Some(_) => now,
        };
        let wait = turn.saturating_duration_since(now);
        (wait, Schedule { period: self.period, next: Some(turn + self.period) })
    }
}

/// `OMNI_FPS_CAP`'s value as a cap: `None` for off.
///
/// # Errors
///
/// A message naming the value when it is not a number of frames a second in `1..=1000`, `0` or
/// `off`.
pub fn parse_cap(text: &str) -> Result<Option<f64>, String> {
    let text = text.trim();
    if text.is_empty() || text == "0" || text.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    match text.parse::<f64>() {
        Ok(fps) if (1.0..=1000.0).contains(&fps) => Ok(Some(fps)),
        _ => Err(format!("OMNI_FPS_CAP={text:?} is not a frame rate in 1..=1000 (or 0/off)")),
    }
}

/// The process's pacer: `None` inside when there is no cap.
static PACER: Mutex<Option<Schedule>> = Mutex::new(None);
static DECIDED: OnceLock<()> = OnceLock::new();
static PACED: AtomicU64 = AtomicU64::new(0);
static WAITED_US: AtomicU64 = AtomicU64::new(0);

fn decide() {
    DECIDED.get_or_init(|| {
        let cap = match std::env::var("OMNI_FPS_CAP") {
            Ok(text) => parse_cap(&text).unwrap_or_else(|why| panic!("{why}")),
            Err(_) => None,
        };
        if let Some(fps) = cap {
            *PACER.lock() = Some(Schedule::new(fps));
            eprintln!(
                "FPS CAP: presents are paced to at most {fps} a second (OMNI_FPS_CAP) -- the engine \
                 renders each frame, later; none is skipped"
            );
        }
    });
}

/// Set the cap for this process, replacing `OMNI_FPS_CAP`'s: for a test, and for an embedding
/// that decides it another way. `None` turns pacing off.
pub fn set_cap(fps: Option<f64>) {
    DECIDED.get_or_init(|| ());
    *PACER.lock() = fps.map(Schedule::new);
}

/// Called at the top of every present (`vkQueuePresentKHR`, `eglSwapBuffers`): wait for this
/// present's turn when there is a cap. One uncontended lock when there is none.
pub fn pace_present() {
    decide();
    let wait = {
        let mut pacer = PACER.lock();
        let Some(schedule) = pacer.as_ref() else { return };
        let (wait, next) = schedule.arrive(Instant::now());
        *pacer = Some(next);
        wait
    };
    PACED.fetch_add(1, Ordering::Relaxed);
    if !wait.is_zero() {
        WAITED_US.fetch_add(u64::try_from(wait.as_micros()).unwrap_or(u64::MAX), Ordering::Relaxed);
        std::thread::sleep(wait);
    }
}

/// How many presents went through a cap, and how long they waited in all.
#[must_use]
pub fn paced() -> (u64, Duration) {
    (PACED.load(Ordering::Relaxed), Duration::from_micros(WAITED_US.load(Ordering::Relaxed)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn presents_are_held_to_their_turns_and_a_stall_restarts_the_schedule() {
        let start = Instant::now();
        let s = Schedule::new(50.0); // 20 ms
        assert_eq!(s.period, 20 * MS);
        // The first goes at once.
        let (wait, s) = s.arrive(start);
        assert_eq!(wait, Duration::ZERO);
        // One 5 ms later waits 15 ms, for its turn at +20.
        let (wait, s) = s.arrive(start + 5 * MS);
        assert_eq!(wait, 15 * MS);
        assert_eq!(s.next, Some(start + 40 * MS));
        // One 8 ms late (+48) goes now, and the next turn stays on the grid (+60).
        let (wait, s) = s.arrive(start + 48 * MS);
        assert_eq!(wait, Duration::ZERO);
        assert_eq!(s.next, Some(start + 60 * MS));
        // A stall: +200 is far more than a period past +60 -- no burst, the grid restarts.
        let (wait, s) = s.arrive(start + 200 * MS);
        assert_eq!(wait, Duration::ZERO);
        assert_eq!(s.next, Some(start + 220 * MS));
        let (wait, _) = s.arrive(start + 201 * MS);
        assert_eq!(wait, 19 * MS);
    }

    #[test]
    fn a_steady_stream_of_presents_averages_the_cap() {
        let start = Instant::now();
        let mut s = Schedule::new(30.0);
        let mut now = start;
        let mut presents = Vec::new();
        for _ in 0..301 {
            // A frame that takes 3 ms to render, then presents.
            now += 3 * MS;
            let (wait, next) = s.arrive(now);
            now += wait;
            presents.push(now);
            s = next;
        }
        let span = presents.last().unwrap().duration_since(presents[0]).as_secs_f64();
        let rate = 300.0 / span;
        assert!((rate - 30.0).abs() < 0.05, "{rate} presents a second");
    }

    #[test]
    fn the_cap_is_parsed_or_refused_by_name() {
        assert_eq!(parse_cap(""), Ok(None));
        assert_eq!(parse_cap("0"), Ok(None));
        assert_eq!(parse_cap("OFF"), Ok(None));
        assert_eq!(parse_cap("15"), Ok(Some(15.0)));
        assert_eq!(parse_cap(" 7.5 "), Ok(Some(7.5)));
        for bad in ["-3", "0.5", "1001", "fast", "nan"] {
            let why = parse_cap(bad).unwrap_err();
            assert!(why.contains("OMNI_FPS_CAP"), "{why}");
        }
    }
}
