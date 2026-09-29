//! **Translations an idle process no longer runs, given back**: the CPU backend keeps every block
//! it ever translated for a process (its shared translation cache, up to 256 MiB live), and a
//! process runs most of its code once -- to start, to boot -- and a small working set after. So
//! the caches only grow: in a Roblox world the system's host process held 712 MiB of translated
//! code beside 357-464 MiB of guest memory, and each idle app process 66-91 MiB of it beside
//! 58-78 MiB (`OMNI_MEM_TRACE`, runs 2026-09-29).
//!
//! Every [`PERIOD`], a process that has been **quiet** -- it translated almost nothing new
//! ([`QUIET_BYTES`]) -- and holds more than [`MIN_BYTES`] of translations has them dropped
//! (`DynarmicBackend::clear_code_cache`): the regions are given back to the OS once its threads
//! have left generated code, and what it runs from then on is translated again, a small working
//! set. At most once a process every [`AGAIN_AFTER`].
//!
//! **Not a busy host process's** (the game's: more than [`BUSY_CORES`] of a core over the period):
//! a clear there would be translated again at once, a hitch for nothing. In the system's host
//! process, **not SurfaceFlinger's**, which runs its frame path every frame. `OMNI_CODE_TRIM=0`
//! turns it off.
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How often processes are looked at.
pub const PERIOD: Duration = Duration::from_secs(60);
/// New translations under this over a period: the process is quiet.
pub const QUIET_BYTES: u64 = 1 << 20;
/// Translations under this are not worth dropping (a region being filled is kept anyway). 8 MiB:
/// the system's host process runs ~60 guest processes, most of them services that translate
/// 8-24 MiB while they start and then sit in a binder wait -- together more than the few large
/// ones a higher floor was set for.
pub const MIN_BYTES: u64 = 8 << 20;
/// A process is trimmed at most this often.
pub const AGAIN_AFTER: Duration = Duration::from_secs(600);
/// A host process using more than this of a core is busy.
pub const BUSY_CORES: f64 = 0.25;

/// Start the trimmer for this host process (once).
pub fn start() {
    if std::env::var("OMNI_CODE_TRIM").as_deref() == Ok("0") {
        return;
    }
    let _ = std::thread::Builder::new().name("omni-code-trim".into()).spawn(run);
}

fn run() {
    // `OMNI_CODE_TRIM_MIN_MB`: the floor, in MiB, instead of [`MIN_BYTES`].
    let min_bytes = std::env::var("OMNI_CODE_TRIM_MIN_MB").ok().and_then(|v| v.parse::<u64>().ok()).map_or(MIN_BYTES, |mb| mb << 20);
    // pid -> (bytes emitted at the last look, when last trimmed).
    let mut seen: HashMap<i32, (u64, Option<Instant>)> = HashMap::new();
    let mut cpu = omni_platform::process::cpu_time().ok();
    loop {
        std::thread::sleep(PERIOD);
        let now_cpu = omni_platform::process::cpu_time().ok();
        let cores = match (cpu, now_cpu) {
            (Some(a), Some(b)) => b.saturating_sub(a).as_secs_f64() / PERIOD.as_secs_f64(),
            _ => f64::MAX,
        };
        cpu = now_cpu;
        let busy = cores > BUSY_CORES && crate::remote::is_remote();
        let live = crate::process::all_live();
        for p in &live {
            let pid = p.sys.pid;
            let emitted = p.code_emitted();
            let (last, trimmed) = seen.get(&pid).copied().unwrap_or((emitted, None));
            seen.insert(pid, (emitted, trimmed));
            let name = String::from_utf8_lossy(&p.comm.lock()).into_owned();
            let committed = p.code_cache_committed();
            let quiet = emitted.saturating_sub(last) < QUIET_BYTES;
            let due = trimmed.is_none_or(|t| t.elapsed() >= AGAIN_AFTER);
            if busy || !quiet || !due || committed < min_bytes || name == "surfaceflinger" {
                continue;
            }
            p.trim_code();
            seen.insert(pid, (emitted, Some(Instant::now())));
            eprintln!("[code] {name} (pid {pid}) quiet: {} MiB of translations dropped", committed >> 20);
        }
        seen.retain(|pid, _| live.iter().any(|p| p.sys.pid == *pid));
    }
}
