//! `OMNI_JIT_TIME=<seconds>`: **how much of a host process's time goes to translating guest code**,
//! once a period while it translates:
//!
//! ```text
//! [jit-time] host process 1234 (com.roblox.client) 5s: +41210 blocks, translate 812 ms, emit 233 ms; total 402118 blocks, 7.9 s + 2.1 s
//! ```
//!
//! `translate` is dynarmic's frontend (decode to IR and its passes), summed over threads, outside the
//! cache's lock; `emit` is x64 emission under the cache's lock (at most one thread's wall time).
//! Both are from the shared code caches' own counters ([`omni_cpu::stats::code_caches`]); a cache
//! that died takes its counts with it. A quiet period prints nothing. Off: no thread.

use std::sync::OnceLock;
use std::time::Duration;

/// The report period, if `OMNI_JIT_TIME` asks for one.
fn period() -> Option<u64> {
    static ON: OnceLock<Option<u64>> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_JIT_TIME").ok().and_then(|v| v.trim().parse().ok()).filter(|&s| s > 0))
}

/// Start the reporter thread if `OMNI_JIT_TIME` is set.
pub fn start() {
    let Some(every) = period() else { return };
    let _ = std::thread::Builder::new().name("omni-jit-time".into()).spawn(move || {
        let mut last = omni_cpu::stats::code_caches();
        loop {
            std::thread::sleep(Duration::from_secs(every));
            let now = omni_cpu::stats::code_caches();
            let blocks = now.blocks_emitted.saturating_sub(last.blocks_emitted);
            if blocks > 0 {
                let name = crate::process::all_live()
                    .iter()
                    .max_by_key(|p| p.code_emitted())
                    .map(|p| String::from_utf8_lossy(&p.comm.lock()).into_owned())
                    .unwrap_or_default();
                eprintln!(
                    "[jit-time] host process {} ({name}) {every}s: +{blocks} blocks, translate {} ms, emit {} ms; total {} blocks, {:.1} s + {:.1} s",
                    std::process::id(),
                    now.translate_ns.saturating_sub(last.translate_ns) / 1_000_000,
                    now.emit_ns.saturating_sub(last.emit_ns) / 1_000_000,
                    now.blocks_emitted,
                    now.translate_ns as f64 / 1e9,
                    now.emit_ns as f64 / 1e9,
                );
            }
            last = now;
        }
    });
}
