//! **Who maps the big pieces** (`OMNI_MMAP_LOG_MB=<n>`; off by default): every guest `mmap`,
//! `mremap` and `munmap` of `n` MiB or more, and every `PR_SET_VMA_ANON_NAME` naming that much,
//! with the thread, what was asked and answered, and where it was called from -- the frame-pointer
//! chain (AOSP and most NDK code keep x29 as one) resolved as `crate::guestprof` resolves its
//! samples: `lib+offset (symbol)`.
//!
//! Made to find the owner of the game's unnamed 1 GiB anonymous reservation (`OMNI_MEM_TRACE`,
//! PS99 in-world 2026-10-09: `anon 0x5b567d000+1024M: 517M` beside `[anon:mimalloc]
//! 0x4eae00000+1024M: 967M`). Lines start `[mmap-log]`; a call's stack follows on the same line
//! after `at`, innermost first (the libc wrapper, then its callers).
use std::sync::{Mutex, OnceLock};

use crate::process::{Process, Task};

/// `OMNI_MMAP_LOG_MB` in bytes, if set.
fn threshold() -> Option<u64> {
    static T: OnceLock<Option<u64>> = OnceLock::new();
    *T.get_or_init(|| std::env::var("OMNI_MMAP_LOG_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).map(|mb| mb.max(1) << 20))
}

/// Whether a call of `len` bytes is logged.
#[must_use]
pub fn wanted(len: u64) -> bool {
    threshold().is_some_and(|t| len >= t)
}

fn millis() -> u128 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis()
}

/// The calling guest code: the system call's pc and lr, then up to 10 frames of the x29 chain,
/// each `lib+offset (symbol)`.
fn stack(p: &Process, t: &Task) -> String {
    static SYMBOLS: OnceLock<Mutex<crate::guestprof::Symbolizer>> = OnceLock::new();
    let mut symbols = SYMBOLS.get_or_init(|| Mutex::new(crate::guestprof::Symbolizer::default())).lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut place = |pc: u64| {
        let pl = symbols.place(p, crate::guest::untag(pc));
        match pl.symbol {
            Some(s) => format!("{}+{:#x} ({s})", pl.lib, pl.offset),
            None => format!("{}+{:#x}", pl.lib, pl.offset),
        }
    };
    let mut out = vec![place(t.pc), place(t.lr)];
    let mut fp = t.fp;
    for _ in 0..10 {
        if fp == 0 || fp % 8 != 0 {
            break;
        }
        let (Ok(next), Ok(lr)) = (p.mem.read_u64(fp), p.mem.read_u64(fp + 8)) else { break };
        if lr == 0 {
            break;
        }
        out.push(place(lr));
        if next <= fp {
            break;
        }
        fp = next;
    }
    out.join(" < ")
}

/// A guest `mmap` and what it answered.
pub fn mmap(p: &Process, t: &Task, a: [u64; 6], r: &Result<u64, crate::errno::Errno>) {
    if !wanted(a[1]) {
        return;
    }
    const MAP_ANONYMOUS: u64 = 0x20;
    let what = if a[3] & MAP_ANONYMOUS != 0 {
        "anonymous".to_string()
    } else {
        p.fds.get(a[4] as i64 as i32).map_or_else(|_| format!("fd {}", a[4] as i64), |f| String::from_utf8_lossy(&crate::fd::guest_path_of(&f)).into_owned())
    };
    let answer = match r {
        Ok(at) => format!("{at:#x}"),
        Err(e) => format!("errno {}", e.0),
    };
    eprintln!(
        "[mmap-log] +{}ms pid {} ({}) tid {} ({}) mmap {} MiB ({:#x}) hint {:#x} prot {} flags {:#x} {what} off {:#x} -> {answer} at {}",
        millis(),
        p.sys.pid,
        String::from_utf8_lossy(&p.comm.lock()),
        t.tid,
        String::from_utf8_lossy(&t.name),
        a[1] >> 20,
        a[1],
        a[0],
        a[2],
        a[3],
        a[5],
        stack(p, t)
    );
}

/// A guest `mremap` (old address and length, new length, flags) and what it answered.
pub fn mremap(p: &Process, t: &Task, a: [u64; 6], r: &Result<u64, crate::errno::Errno>) {
    if !wanted(a[1].max(a[2])) {
        return;
    }
    let answer = match r {
        Ok(at) => format!("{at:#x}"),
        Err(e) => format!("errno {}", e.0),
    };
    eprintln!(
        "[mmap-log] +{}ms pid {} tid {} ({}) mremap {:#x} {} -> {} MiB flags {:#x} -> {answer} at {}",
        millis(),
        p.sys.pid,
        t.tid,
        String::from_utf8_lossy(&t.name),
        a[0],
        a[1] >> 20,
        a[2] >> 20,
        a[3],
        stack(p, t)
    );
}

/// A guest `munmap`.
pub fn munmap(p: &Process, t: &Task, a: [u64; 6]) {
    if !wanted(a[1]) {
        return;
    }
    eprintln!("[mmap-log] +{}ms pid {} tid {} munmap {:#x}+{} MiB at {}", millis(), p.sys.pid, t.tid, a[0], a[1] >> 20, stack(p, t));
}

/// `PR_SET_VMA_ANON_NAME` over `[start, start + len)`.
pub fn named(p: &Process, t: &Task, start: u64, len: u64, name: &[u8]) {
    if !wanted(len) {
        return;
    }
    eprintln!(
        "[mmap-log] +{}ms pid {} tid {} named {start:#x}+{} MiB \"{}\" at {}",
        millis(),
        p.sys.pid,
        t.tid,
        len >> 20,
        String::from_utf8_lossy(name),
        stack(p, t)
    );
}
