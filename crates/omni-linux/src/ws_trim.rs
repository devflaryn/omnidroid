//! `OMNI_WS_TRIM=<seconds>` (off by default): every period, this host process's working set is
//! emptied (`omni_platform::vm::trim_working_set`). A game keeps much of its memory untouched once
//! loaded (assets decoded once, caches); those pages then leave the private working set -- what
//! Task Manager shows -- for the system's compressed store or the page file, and the pages still in
//! use come back at their next touch as soft faults. Commit charge does not change. Each trim says
//! what it did:
//!
//! ```text
//! [ws-trim] host process 1234: working set 2310 -> 140 MiB (12 ms)
//! ```
//!
//! The first trim is a period after the start. `OMNI_WS_TRIM_ONCE=1`: only that one.

use std::time::{Duration, Instant};

/// Start the trimming thread if `OMNI_WS_TRIM` asks for one.
pub fn start() {
    let Some(every) = std::env::var("OMNI_WS_TRIM").ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|&s| s > 0) else {
        return;
    };
    let once = std::env::var("OMNI_WS_TRIM_ONCE").is_ok_and(|v| v.trim() == "1");
    let _ = std::thread::Builder::new().name("omni-ws-trim".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_secs(every));
        let before = omni_platform::vm::process_working_set().unwrap_or(0) >> 20;
        let t = Instant::now();
        let ok = omni_platform::vm::trim_working_set();
        let took = t.elapsed().as_millis();
        let after = omni_platform::vm::process_working_set().unwrap_or(0) >> 20;
        eprintln!("[ws-trim] host process {}: working set {before} -> {after} MiB ({took} ms){}", std::process::id(), if ok { "" } else { ", refused" });
        if once {
            return;
        }
    });
}
