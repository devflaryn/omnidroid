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
    age::start();
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
    let started = Instant::now();
    // `first_trim` (`crate::settle`): the first look when the host process has settled after its
    // start -- quiet, in all its processes, for seconds in a row -- rather than a period after it.
    // Every process is then quiet by that measure; the size floor and the rest still apply.
    let mut cpu = omni_platform::process::cpu_time().ok();
    let mut since = started;
    let mut early = false;
    while started.elapsed() < PERIOD {
        std::thread::sleep(Duration::from_secs(1));
        if crate::settle::FIRST_TRIM.load(std::sync::atomic::Ordering::Relaxed) && crate::settle::settled() {
            look(&mut seen, false, true, min_bytes);
            (cpu, since, early) = (omni_platform::process::cpu_time().ok(), Instant::now(), true);
            break;
        }
    }
    // Then a look every period (the first at the period after the start when there was no early
    // one, as before).
    let mut first = !early;
    loop {
        if !first {
            std::thread::sleep(PERIOD);
        }
        first = false;
        let now_cpu = omni_platform::process::cpu_time().ok();
        let cores = match (cpu, now_cpu) {
            (Some(a), Some(b)) => b.saturating_sub(a).as_secs_f64() / since.elapsed().as_secs_f64().max(1.0),
            _ => f64::MAX,
        };
        (cpu, since) = (now_cpu, Instant::now());
        let busy = cores > BUSY_CORES && crate::remote::is_remote();
        look(&mut seen, busy, false, min_bytes);
    }
}

/// One look over the live processes: each quiet one (or every one, `all_quiet`) holding more than
/// `min_bytes` of translations, not trimmed in [`AGAIN_AFTER`], has them dropped -- unless `busy`.
fn look(seen: &mut HashMap<i32, (u64, Option<Instant>)>, busy: bool, all_quiet: bool, min_bytes: u64) {
    let live = crate::process::all_live();
    for p in &live {
        let pid = p.sys.pid;
        let emitted = p.code_emitted();
        let (last, trimmed) = seen.get(&pid).copied().unwrap_or((emitted, None));
        seen.insert(pid, (emitted, trimmed));
        let name = String::from_utf8_lossy(&p.comm.lock()).into_owned();
        let committed = p.code_cache_committed();
        let quiet = all_quiet || emitted.saturating_sub(last) < QUIET_BYTES;
        let due = trimmed.is_none_or(|t| t.elapsed() >= AGAIN_AFTER);
        if busy || !quiet || !due || committed < min_bytes || name == "surfaceflinger" {
            continue;
        }
        p.trim_code();
        seen.insert(pid, (emitted, Some(Instant::now())));
        eprintln!("[code] {name} (pid {pid}) quiet{}: {} MiB of translations dropped", if all_quiet { " (settled after its start)" } else { "" }, committed >> 20);
    }
    seen.retain(|pid, _| live.iter().any(|p| p.sys.pid == *pid));
}

/// **The age pass: the oldest translations of the system's busy processes, given back on a long
/// period** (`OMNI_CODE_AGE=<minutes>` / lever `code_age=<minutes>`; 0, the default, is off).
///
/// The trim above never reaches a process that keeps translating a little -- system_server in a
/// world (`[mem]` 2026-10-09: 86 MiB of translations after its one quiet trim, 119 MiB later) --
/// and never SurfaceFlinger (65 MiB); most of either is code run once, at boot or at the app's
/// start. A shared cache fills regions in order and patch 0028 already retires the oldest when a
/// live limit is reached; this asks for the same, on demand (patch 0050, `Process::age_code`):
/// every period, each process of the system's host process holding more than the kept size
/// (`OMNI_CODE_AGE_KEEP_MB` / `code_age_keep=<MiB>`, 32 by default) has its oldest regions retired,
/// one at a time with a pause between, down to it. What still runs in them is translated again,
/// once, into the region being filled -- the hot set moves forward, the cold code goes.
///
/// Measured offline (`dynarmic-sys` `shared_cache.rs::an_eviction_on_demand_...`): a region's
/// eviction ~23 ms under the cache's lock (4 regions, 315k small blocks: 92 ms), a block translated
/// again ~9 us (small blocks; a real one costs more). Only in the system's host process (an app's
/// is the game's: never); SurfaceFlinger only with `OMNI_CODE_AGE_SF=1` / `code_age_sf=1` -- a
/// retranslation burst on its thread is a frame missed.
pub mod age {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// Minutes between passes; 0: none.
    pub static MINUTES: AtomicU64 = AtomicU64::new(0);
    /// MiB of translations a process keeps.
    pub static KEEP_MB: AtomicU64 = AtomicU64::new(32);
    /// SurfaceFlinger too -- **on by default** since the in-world soak of 2026-10-09 (PS99,
    /// `OMNI_CODE_AGE=5`: surfaceflinger 54 -> 25 MiB of translations in a 420 ms pass, fps 59.1-60.1
    /// in the 5 s windows around it). `OMNI_CODE_AGE_SF=0` or the lever leaves it alone.
    pub static SURFACEFLINGER: AtomicBool = AtomicBool::new(true);
    static KICK: AtomicBool = AtomicBool::new(false);
    /// An app's host process (the game's) too, this many minutes after it starts (then again only
    /// past [`GAME_SLACK_MB`]); 0: never. Its oldest regions hold its start-up and sign-in code, run
    /// once; what it still runs is translated again into a new region. **3 by default** since the
    /// in-world runs of 2026-10-09 (PS99 s20-s22: its translations 234-242 -> 84-92 MiB, private
    /// working set 3.37 -> 3.12-3.22 GB with the table shrink; one pass a session, ~15 s at 47-57
    /// fps while it retires its regions a second apart). `OMNI_CODE_AGE_GAME=0` or the lever
    /// `code_age_game=0` turns it off.
    pub static GAME_MINUTES: AtomicU64 = AtomicU64::new(3);
    /// MiB of translations the game keeps (`OMNI_CODE_AGE_GAME_KEEP_MB`, `code_age_game_keep`).
    pub static GAME_KEEP_MB: AtomicU64 = AtomicU64::new(96);
    static KICK_GAME: AtomicBool = AtomicBool::new(false);
    /// After the game's first pass, another only once its translations have grown this many MiB
    /// past the kept size (`OMNI_CODE_AGE_GAME_SLACK_MB`, default 48): a new place's code after a
    /// teleport. A pass retires the oldest regions, and after the first those hold the code the
    /// game still runs, translated again into them -- every later pass on a period evicted it
    /// again, a hitch for ~30 MiB (PS99 s21: 141 -> 84 MiB, a 5 s window at 37 fps).
    pub static GAME_SLACK_MB: AtomicU64 = AtomicU64::new(48);
    /// Milliseconds between the regions a game pass retires (`OMNI_CODE_AGE_GAME_PAUSE_MS`, default
    /// 1000): what it still runs is translated again a region at a time, not all at once (s21, 200
    /// ms: the first pass's 5 s window at 46.9 fps).
    pub static GAME_PAUSE_MS: AtomicU64 = AtomicU64::new(1000);
    /// Whether this host process's game has had its first pass.
    static GAME_PASSED: AtomicBool = AtomicBool::new(false);

    /// The game's period, from the lever; a period set starts with a pass.
    pub fn set_game_minutes(m: u64) {
        GAME_MINUTES.store(m, Ordering::Relaxed);
        KICK_GAME.store(m != 0, Ordering::Relaxed);
    }

    /// The period, from the lever; a period set starts with a pass.
    pub fn set_minutes(m: u64) {
        MINUTES.store(m, Ordering::Relaxed);
        KICK.store(m != 0, Ordering::Relaxed);
    }

    /// Read the environment and start the pass's thread (once). It passes only in the system's
    /// host process (an app's host process is known as one -- `remote::is_remote` -- only once its
    /// binder is connected, after this starts, so the thread asks every time).
    pub fn start() {
        if let Some(mb) = std::env::var("OMNI_CODE_AGE_KEEP_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
            KEEP_MB.store(mb, Ordering::Relaxed);
        }
        if let Ok(v) = std::env::var("OMNI_CODE_AGE_SF") {
            SURFACEFLINGER.store(v.trim() != "0", Ordering::Relaxed);
        }
        // Every 10 minutes by default since the in-world run of 2026-10-09 (PS99 session s10:
        // system_server 118 -> 29 MiB of translations on the first pass (1.3 s of its CPU), 41 -> 26
        // on the next (0.2 s); the system host's private working set ~1.12 -> ~1.0 GB with the table
        // compaction). `OMNI_CODE_AGE=0` or the lever `code_age=0` turns it off.
        let m = std::env::var("OMNI_CODE_AGE").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(10);
        set_minutes(m);
        if let Some(mb) = std::env::var("OMNI_CODE_AGE_GAME_KEEP_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
            GAME_KEEP_MB.store(mb, Ordering::Relaxed);
        }
        if let Some(g) = std::env::var("OMNI_CODE_AGE_GAME").ok().and_then(|v| v.parse::<u64>().ok()) {
            GAME_MINUTES.store(g, Ordering::Relaxed);
        }
        if let Some(mb) = std::env::var("OMNI_CODE_AGE_GAME_SLACK_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
            GAME_SLACK_MB.store(mb, Ordering::Relaxed);
        }
        if let Some(ms) = std::env::var("OMNI_CODE_AGE_GAME_PAUSE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            GAME_PAUSE_MS.store(ms, Ordering::Relaxed);
        }
        if std::env::var_os("OMNI_CODE_AGE").is_some() && m != 0 {
            eprintln!("[lever] OMNI_CODE_AGE: code_age={m} (keep {} MiB)", KEEP_MB.load(Ordering::Relaxed));
        }
        let _ = std::thread::Builder::new().name("omni-code-age".into()).spawn(|| {
            let mut last = Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let game = crate::remote::is_remote();
                let (minutes, kick, keep) = if game {
                    (GAME_MINUTES.load(Ordering::Relaxed), &KICK_GAME, GAME_KEEP_MB.load(Ordering::Relaxed))
                } else {
                    (MINUTES.load(Ordering::Relaxed), &KICK, KEEP_MB.load(Ordering::Relaxed))
                };
                if minutes == 0 {
                    continue;
                }
                if !kick.swap(false, Ordering::Relaxed) && last.elapsed() < Duration::from_secs(minutes * 60) {
                    continue;
                }
                last = Instant::now();
                if game {
                    let slack = if GAME_PASSED.load(Ordering::Relaxed) { GAME_SLACK_MB.load(Ordering::Relaxed) << 20 } else { 0 };
                    if pass(keep << 20, slack, Duration::from_millis(GAME_PAUSE_MS.load(Ordering::Relaxed))) > 0 {
                        GAME_PASSED.store(true, Ordering::Relaxed);
                    }
                } else {
                    pass(keep << 20, 0, Duration::from_millis(200));
                }
            }
        });
    }

    /// One pass over the live processes, each keeping `keep` bytes of translations, `pause` between
    /// the regions it retires; a process holding no more than `keep + slack` is left alone. The
    /// regions retired, over all.
    pub fn pass(keep: u64, slack: u64, pause: Duration) -> usize {
        let mut total = 0;
        for p in crate::process::all_live() {
            let name = String::from_utf8_lossy(&p.comm.lock()).into_owned();
            if name == "surfaceflinger" && !SURFACEFLINGER.load(Ordering::Relaxed) {
                continue;
            }
            let before = p.code_cache_committed();
            if before <= keep + slack {
                continue;
            }
            let t = Instant::now();
            let retired = p.age_code(keep, pause);
            total += retired as usize;
            if retired > 0 {
                eprintln!(
                    "[code] age: {name} (pid {}) {} -> {} MiB of translations, {retired} regions retired ({} ms)",
                    p.sys.pid,
                    before >> 20,
                    p.code_cache_committed() >> 20,
                    t.elapsed().as_millis()
                );
            }
        }
        total
    }
}
