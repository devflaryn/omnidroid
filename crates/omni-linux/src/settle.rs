//! **When a host process has settled after its start**: its first zero sweep
//! ([`crate::zero_reclaim`]) and, with `first_trim`, its first code trim ([`crate::code_trim`])
//! run then rather than a whole period (60 s) after the start.
//!
//! Why: a new app host process (networkstack, media.module, ext.services, the webview sandbox, ...)
//! sits at 110-168 MB of private working set for its first minute and drops to 47-57 MB once the
//! zero sweep and the code trim have run (RAM agent, PS99 session of 2026-10-09; 71-81 MB with only
//! the code trim, before the zero sweep was on). Several start together at boot.
//!
//! **Settled** = at least [`FIRST_S`] seconds since the host process started (`first_sweep=<s>`,
//! `OMNI_FIRST_SWEEP=<s>`; 12 by default; 0: never early, the period as before) **and**
//! [`QUIET_SECONDS`] seconds in a row each with under [`QUIET_CORES`] of a core of this host
//! process's CPU and under [`QUIET_EMIT`] bytes of new translations in all its guest processes. So
//! nothing is swept or trimmed while a process is still starting (translating, running its startup
//! work), and a process that never quiets keeps the plain period. Looked at once a second, until
//! settled; at most [`GIVE_UP`] after the start.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Seconds after the start before a host process may count as settled; 0: never early.
pub static FIRST_S: AtomicU64 = AtomicU64::new(12);
/// `first_trim=0|1` (`OMNI_FIRST_TRIM=1`): the code trim's first look at the settle too. **Off by
/// default**: dropping translations early costs translating again if the process wakes before its
/// minute is out.
pub static FIRST_TRIM: AtomicBool = AtomicBool::new(false);
/// A quiet second: under this much of a core ...
pub const QUIET_CORES: f64 = 0.15;
/// ... and under this many bytes of new translations.
pub const QUIET_EMIT: u64 = 128 << 10;
/// Quiet seconds in a row that make a process settled.
pub const QUIET_SECONDS: u32 = 3;
/// The detector stops looking this long after the start (the periods take over).
pub const GIVE_UP: Duration = Duration::from_secs(120);

static SETTLED: AtomicBool = AtomicBool::new(false);

/// Whether this host process has settled after its start.
#[must_use]
pub fn settled() -> bool {
    SETTLED.load(Ordering::Relaxed)
}

/// The detector: fed one second's look at a time.
#[derive(Debug, Clone, Copy)]
pub struct Settle {
    after: Duration,
    quiet_run: u32,
}

impl Settle {
    /// Settled no sooner than `after` from the start (zero: never).
    #[must_use]
    pub fn new(after: Duration) -> Self {
        Self { after, quiet_run: 0 }
    }

    /// One second's look: `since_start`, the host process's CPU over that second (in cores) and
    /// the bytes its guest processes translated in it. True once settled.
    pub fn observe(&mut self, since_start: Duration, cores: f64, emitted: u64) -> bool {
        let quiet = cores < QUIET_CORES && emitted < QUIET_EMIT;
        self.quiet_run = if quiet { self.quiet_run + 1 } else { 0 };
        !self.after.is_zero() && since_start >= self.after && self.quiet_run >= QUIET_SECONDS
    }
}

/// The bytes translated so far by every live guest process of this host process.
fn emitted_total() -> u64 {
    crate::process::all_live().iter().map(|p| p.code_emitted()).sum()
}

/// Start the detector (once); `started` is when the host process started.
pub fn start(started: Instant) {
    if let Some(s) = std::env::var("OMNI_FIRST_SWEEP").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        FIRST_S.store(s, Ordering::Relaxed);
        eprintln!("[lever] OMNI_FIRST_SWEEP: first_sweep={s}");
    }
    if std::env::var("OMNI_FIRST_TRIM").as_deref() == Ok("1") {
        FIRST_TRIM.store(true, Ordering::Relaxed);
        eprintln!("[lever] OMNI_FIRST_TRIM: first_trim=1");
    }
    let _ = std::thread::Builder::new().name("omni-settle".into()).spawn(move || {
        let (mut cpu, mut emitted) = (omni_platform::process::cpu_time().ok(), emitted_total());
        let mut settle = Settle::new(Duration::from_secs(FIRST_S.load(Ordering::Relaxed)));
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if started.elapsed() >= GIVE_UP {
                return;
            }
            let (now_cpu, now_emitted) = (omni_platform::process::cpu_time().ok(), emitted_total());
            let cores = match (cpu, now_cpu) {
                (Some(a), Some(b)) => b.saturating_sub(a).as_secs_f64(),
                _ => f64::MAX,
            };
            let new = now_emitted.saturating_sub(emitted);
            (cpu, emitted) = (now_cpu, now_emitted);
            // The lever may have moved the delay meanwhile.
            let after = Duration::from_secs(FIRST_S.load(Ordering::Relaxed));
            if after != settle.after {
                settle.after = after;
            }
            if settle.observe(started.elapsed(), cores, new) {
                SETTLED.store(true, Ordering::Relaxed);
                eprintln!("[settle] host pid {}: settled {:.0} s after its start", std::process::id(), started.elapsed().as_secs_f64());
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(settle: &mut Settle, seconds: std::ops::Range<u64>, cores: f64, emitted: u64) -> Option<u64> {
        seconds.into_iter().find(|&s| settle.observe(Duration::from_secs(s), cores, emitted))
    }

    #[test]
    fn a_quiet_process_settles_at_the_delay_and_not_before() {
        let mut s = Settle::new(Duration::from_secs(12));
        assert_eq!(run(&mut s, 1..30, 0.01, 0), Some(12), "quiet from the start: at the delay");
    }

    #[test]
    fn a_busy_start_is_waited_out_and_any_busy_second_starts_the_count_again() {
        let mut s = Settle::new(Duration::from_secs(12));
        // Translating hard for 20 s (the startup burst): never settled meanwhile.
        assert_eq!(run(&mut s, 1..21, 0.9, 8 << 20), None);
        // Quiet from 21: settled on the third quiet second.
        assert_eq!(run(&mut s, 21..22, 0.01, 0), None);
        assert_eq!(run(&mut s, 22..23, 0.01, 0), None);
        // A burst of translation alone (little CPU) also counts as busy.
        assert_eq!(run(&mut s, 23..24, 0.01, QUIET_EMIT), None);
        assert_eq!(run(&mut s, 24..40, 0.05, 1024), Some(26));
    }

    #[test]
    fn a_delay_of_zero_never_settles_early() {
        let mut s = Settle::new(Duration::ZERO);
        assert_eq!(run(&mut s, 1..100, 0.0, 0), None);
    }
}
