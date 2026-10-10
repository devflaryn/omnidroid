//! The host process's working set, emptied (`omni_platform::vm::trim_working_set`). A game keeps
//! much of its memory untouched once loaded (assets decoded once, caches), and Android's helper apps
//! sit idle once the device is up; those pages leave the private working set -- what Task Manager
//! shows -- for the system's compressed store or the page file, and the pages still in use come back
//! at their next touch as soft faults. Commit charge does not change. Measured (2026-10-10 s8, PS99,
//! a trim every 120 s): two minutes after a trim the system's host had touched 187 of its 1,294 MB
//! again, the game 748 of 3,011, each idle helper app 2 of ~150.
//!
//! - `OMNI_WS_TRIM=<seconds>` (**default 120**; `0` off): every period, whatever the process is
//!   doing. Measured (s10, PS99): the machine's available memory 9.21 -> 11.13 GB (+1.9 GB; the
//!   system's compressed store +1.0 GB of it), private working set 3.02 -> 0.73 GB, with fps, CPU a
//!   frame and the vsync pacer's lateness unchanged.
//! - `OMNI_WS_TRIM_IDLE=<seconds>` (**default 30**; `0` off): once the process has used under 2% of
//!   a core for that long (a helper app waiting in binder, a game in the background), and holds
//!   more than 8 MiB; never in its first minute. Again after it has grown back past 8 MiB and gone
//!   idle again. Measured (s10): Android's five idle helper apps 205-266 -> 0-12 MB each.
//!
//! Each trim says what it did:
//!
//! ```text
//! [ws-trim] host process 1234: working set 2310 -> 140 MiB (12 ms)
//! ```
//!
//! `OMNI_WS_TRIM_ONCE=1`: only the first periodic trim.

use std::time::{Duration, Instant};

/// A process using less than this share of a core is idle.
const IDLE_SHARE: f64 = 0.02;
/// Under this working set there is nothing worth trimming.
const IDLE_MIN_BYTES: u64 = 8 << 20;
/// No idle trim before the process is this old: its start is busy, then quiet for a moment.
const IDLE_NOT_BEFORE: Duration = Duration::from_secs(60);
/// How often the idle trimmer looks.
const IDLE_LOOK: Duration = Duration::from_secs(5);

/// The period `var` asks for, else `default`; 0 is off.
fn seconds(var: &str, default: u64) -> Option<u64> {
    let s = std::env::var(var).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default);
    (s > 0).then_some(s)
}

fn trim(why: &str) {
    let before = omni_platform::vm::process_working_set().unwrap_or(0) >> 20;
    let t = Instant::now();
    let ok = omni_platform::vm::trim_working_set();
    let took = t.elapsed().as_millis();
    let after = omni_platform::vm::process_working_set().unwrap_or(0) >> 20;
    eprintln!("[ws-trim] host process {}{why}: working set {before} -> {after} MiB ({took} ms){}", std::process::id(), if ok { "" } else { ", refused" });
}

/// Start the trimming threads `OMNI_WS_TRIM` / `OMNI_WS_TRIM_IDLE` ask for.
pub fn start() {
    if let Some(every) = seconds("OMNI_WS_TRIM", 120) {
        let once = std::env::var("OMNI_WS_TRIM_ONCE").is_ok_and(|v| v.trim() == "1");
        let _ = std::thread::Builder::new().name("omni-ws-trim".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_secs(every));
            trim("");
            if once {
                return;
            }
        });
    }
    if let Some(idle_for) = seconds("OMNI_WS_TRIM_IDLE", 30) {
        let _ = std::thread::Builder::new().name("omni-ws-trim-idle".into()).spawn(move || {
            let started = Instant::now();
            let window = Duration::from_secs(idle_for);
            // (when, CPU time then): the samples over the last `window`.
            let mut samples: std::collections::VecDeque<(Instant, Duration)> = std::collections::VecDeque::new();
            let mut trimmed = false;
            loop {
                std::thread::sleep(IDLE_LOOK);
                let now = Instant::now();
                let Ok(cpu) = omni_platform::process::cpu_time() else { continue };
                samples.push_back((now, cpu));
                while samples.front().is_some_and(|(t, _)| now.duration_since(*t) > window) {
                    samples.pop_front();
                }
                let Some(&(then, cpu_then)) = samples.front() else { continue };
                let span = now.duration_since(then);
                if span + IDLE_LOOK < window || started.elapsed() < IDLE_NOT_BEFORE {
                    continue;  // not looked at long enough yet
                }
                let share = cpu.saturating_sub(cpu_then).as_secs_f64() / span.as_secs_f64().max(1e-3);
                let ws = omni_platform::vm::process_working_set().unwrap_or(0);
                if ws < IDLE_MIN_BYTES {
                    trimmed = false;  // below the floor: the next growth may be trimmed again
                    continue;
                }
                if share < IDLE_SHARE && !trimmed {
                    trim(&format!(" (idle: {:.1}% of a core for {} s)", share * 100.0, span.as_secs()));
                    trimmed = true;
                } else if share >= IDLE_SHARE {
                    trimmed = false;  // busy again: once idle again, trim again
                }
            }
        });
    }
}
