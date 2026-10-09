//! **Guest pages that hold nothing but zeros, given back to the host as RAM** (Windows).
//!
//! On Linux a page a program reads before it writes is the one shared zero page; on Windows the
//! first touch of committed memory -- a read as much as a write, or a `memset` of zeros -- gives the
//! host process a private page, and it stays in the private working set. MEASURED (PS99 in-world,
//! 2026-10-08, `wsscan.ps1`, `main` `6b1ab88`): 169 MiB of the game's host process's guest memory
//! and 10 MiB of the system's were resident pages of zeros.
//!
//! Every period, each guest process's space is swept (`omni_mem::GuestSpace::reset_zero_pages`):
//! a resident page that reads zero is taken out of the working set with its contents disposable,
//! guarded against a concurrent write (`omni_platform::vm::reset_zero_run`). What the guest reads
//! does not change; its next touch of such a page reads zeros again. Commit charge is not
//! returned (the pages stay committed), only RAM. Cost: a working-set query per 1 MiB and a read of
//! each resident page's first words (a page that is not zero says so at once) -- ~20-40 ms a sweep
//! for ~1.6 GiB of guest memory, off every guest thread.
//!
//! **The first sweep** comes when the host process has settled after its start (`crate::settle`:
//! 12 s at the soonest, `first_sweep=<s>`; 0 waits the period as before), then every period.
//!
//! **On by default (60 s)**: `OMNI_ZERO_RECLAIM=<seconds>` from the start (0: off), or the live lever
//! `zero_reclaim=<seconds>` (`crate::lever`; 0 stops it). Each sweep that finds something says so:
//! `[zero] pid <host pid>: <n> MiB of zero pages reset of <m> MiB resident guest memory (<k>
//! processes, <t> ms)`.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Seconds between sweeps; 0: none.
pub static PERIOD_S: AtomicU64 = AtomicU64::new(0);

/// The period a host process sweeps with unless told otherwise.
pub const DEFAULT_PERIOD_S: u64 = 60;
/// Set by the lever: sweep at the next look rather than a whole period later.
static KICK: AtomicBool = AtomicBool::new(false);

/// Set the period (the lever and the environment); a period set starts with a sweep.
pub fn set_period(seconds: u64) {
    PERIOD_S.store(seconds, Ordering::Relaxed);
    KICK.store(seconds != 0, Ordering::Relaxed);
}

/// Start the sweeper for this host process (once). It idles while the period is 0.
pub fn start() {
    // A sweep a minute by default since the in-world A/B of 2026-10-09 (PS99: the first sweep
    // gave back ~126 MB of working set; a steady sweep costs ~0.1 s a minute in the game's host
    // process); `OMNI_ZERO_RECLAIM=0` or the lever turns it off.
    let s = std::env::var("OMNI_ZERO_RECLAIM").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(DEFAULT_PERIOD_S);
    set_period(s);
    if std::env::var_os("OMNI_ZERO_RECLAIM").is_some() {
        eprintln!("[lever] OMNI_ZERO_RECLAIM: zero_reclaim={s}");
    }
    // The first sweep once the host process has settled after its start (`crate::settle`, 12 s at
    // the soonest), not a whole period after it: a new app host process holds its startup's zero
    // pages for that minute otherwise.
    crate::settle::start(Instant::now());
    let _ = std::thread::Builder::new().name("omni-zero-reclaim".into()).spawn(|| {
        let mut last = Instant::now();
        let mut first_done = false;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let period = PERIOD_S.load(Ordering::Relaxed);
            if period == 0 {
                continue;
            }
            let early = !first_done && crate::settle::settled();
            if !early && !KICK.swap(false, Ordering::Relaxed) && last.elapsed() < Duration::from_secs(period) {
                continue;
            }
            first_done = true;
            last = Instant::now();
            sweep_and_report();
        }
    });
}

/// One sweep of every live guest process of this host process: what was found, summed.
pub fn sweep() -> (omni_mem::ZeroPages, usize) {
    let live = crate::process::all_live();
    let mut seen: Vec<*const omni_mem::GuestSpace> = Vec::new();
    let mut total = omni_mem::ZeroPages::default();
    for p in &live {
        let space = p.mem.space();
        let key = std::sync::Arc::as_ptr(space);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        match space.reset_zero_pages() {
            Ok(found) => total = total.add(found),
            Err(e) => eprintln!("[zero] pid {}: {e}", std::process::id()),
        }
    }
    (total, seen.len())
}

fn sweep_and_report() {
    let t0 = Instant::now();
    let (found, spaces) = sweep();
    if found.reset == 0 {
        return;
    }
    let mib = |pages: usize| (pages * omni_mem::SMALL_PAGE) >> 20;
    eprintln!(
        "[zero] pid {}: {} MiB of zero pages reset of {} MiB resident guest memory ({spaces} processes, {} ms)",
        std::process::id(),
        mib(found.reset),
        mib(found.resident),
        t0.elapsed().as_millis()
    );
}
