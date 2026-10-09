//! **Who holds the host heap's large blocks** (`OMNI_ALLOC_TRACE_KB=<n>`; off by default): every
//! live Rust allocation of `n` KiB or more, with the call stack that made it, reported every 60 s
//! as `[alloc]` lines grouped by stack -- count, size, live bytes, the addresses (to match
//! `tools/zeroscan.ps1`'s allocation bases: a large heap block's base is its address rounded down
//! to 64 KiB) and the stack resolved through the executable's PDB.
//!
//! The global allocator of `omni-linux-run` ([`Tracing`]) is the system's with one relaxed load
//! and a compare added to every allocation while the trace is off. On, a large allocation takes a
//! lock and an unwind (`RtlCaptureStackBackTrace`, ~1-2 us): fine for a measurement session, not
//! for an A/B. C++ allocations (dynarmic's tables: `OMNI_MEM_TRACE` names those) and the GPU
//! driver's do not pass through it.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The threshold in bytes; 0: off.
static THRESHOLD: AtomicUsize = AtomicUsize::new(0);

const FRAMES: usize = 20;
const SLOTS: usize = 8192;

#[derive(Clone, Copy)]
struct Entry {
    ptr: usize,
    size: usize,
    frames: [usize; FRAMES],
}

const EMPTY: Entry = Entry { ptr: 0, size: 0, frames: [0; FRAMES] };

/// The live large allocations (open addressing by pointer). `parking_lot`'s lock allocates
/// nothing; the table is static and touched only while the trace is on.
static TABLE: parking_lot::Mutex<[Entry; SLOTS]> = parking_lot::Mutex::new([EMPTY; SLOTS]);

fn slot_of(ptr: usize) -> usize {
    (ptr >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - 13)
}

fn track(ptr: usize, size: usize) {
    let mut frames = [0usize; FRAMES];
    let _ = omni_platform::sampler::capture_return_addresses(2, &mut frames);
    let mut table = TABLE.lock();
    let mut i = slot_of(ptr) % SLOTS;
    for _ in 0..SLOTS {
        if table[i].ptr == 0 || table[i].ptr == ptr {
            table[i] = Entry { ptr, size, frames };
            return;
        }
        i = (i + 1) % SLOTS;
    }
}

fn untrack(ptr: usize) {
    let mut table = TABLE.lock();
    let mut i = slot_of(ptr) % SLOTS;
    for _ in 0..SLOTS {
        if table[i].ptr == ptr {
            // Backward-shift deletion keeps every probe chain whole.
            let mut hole = i;
            let mut j = (i + 1) % SLOTS;
            loop {
                if table[j].ptr == 0 {
                    break;
                }
                let home = slot_of(table[j].ptr) % SLOTS;
                let between = if hole <= j { home <= hole || home > j } else { home <= hole && home > j };
                if between {
                    table[hole] = table[j];
                    hole = j;
                }
                j = (j + 1) % SLOTS;
            }
            table[hole] = EMPTY;
            return;
        }
        if table[i].ptr == 0 {
            return;
        }
        i = (i + 1) % SLOTS;
    }
}

/// `omni-linux-run`'s global allocator: the system's, tracing large allocations when asked.
pub struct Tracing;

// SAFETY: every call is the system allocator's, with the same layout; the tracing only records.
unsafe impl GlobalAlloc for Tracing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as the caller's contract.
        let p = unsafe { System.alloc(layout) };
        let t = THRESHOLD.load(Ordering::Relaxed);
        if t != 0 && layout.size() >= t && !p.is_null() {
            track(p as usize, layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as the caller's contract.
        let p = unsafe { System.alloc_zeroed(layout) };
        let t = THRESHOLD.load(Ordering::Relaxed);
        if t != 0 && layout.size() >= t && !p.is_null() {
            track(p as usize, layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let t = THRESHOLD.load(Ordering::Relaxed);
        if t != 0 && layout.size() >= t {
            untrack(ptr as usize);
        }
        // SAFETY: as the caller's contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let t = THRESHOLD.load(Ordering::Relaxed);
        if t != 0 && layout.size() >= t {
            untrack(ptr as usize);
        }
        // SAFETY: as the caller's contract.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if t != 0 && new_size >= t && !p.is_null() {
            track(p as usize, new_size);
        } else if t != 0 && layout.size() >= t && p.is_null() {
            track(ptr as usize, layout.size()); // the old block stays
        }
        p
    }
}

/// Read `OMNI_ALLOC_TRACE_KB` and, when set, start tracing and the report thread (once, from
/// `main`).
pub fn start() {
    let Some(kb) = std::env::var("OMNI_ALLOC_TRACE_KB").ok().and_then(|v| v.trim().parse::<usize>().ok()) else { return };
    THRESHOLD.store(kb.max(1) << 10, Ordering::Relaxed);
    eprintln!("[alloc] pid {}: tracing live allocations of {kb} KiB or more", std::process::id());
    let _ = std::thread::Builder::new().name("omni-alloc-trace".into()).spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        report();
    });
}

/// How many large allocations are live, and their bytes (for a test).
#[must_use]
pub fn live() -> (usize, usize) {
    let table = TABLE.lock();
    table.iter().filter(|e| e.ptr != 0).fold((0, 0), |(n, b), e| (n + 1, b + e.size))
}

/// One `[alloc]` line per call stack, most live bytes first (the top 25).
pub fn report() {
    // Room made before the lock: allocating under it would trace this very buffer and take the
    // lock again.
    let mut live: Vec<Entry> = Vec::with_capacity(SLOTS);
    {
        let table = TABLE.lock();
        let own = live.as_ptr() as usize; // this buffer is itself a traced block
        live.extend(table.iter().filter(|e| e.ptr != 0 && e.ptr != own).copied());
    }
    let mut by: std::collections::HashMap<[usize; FRAMES], (usize, usize, Vec<usize>)> = std::collections::HashMap::new();
    for e in &live {
        let g = by.entry(e.frames).or_default();
        g.0 += 1;
        g.1 += e.size;
        if g.2.len() < 4 {
            g.2.push(e.ptr);
        }
    }
    let mut rows: Vec<_> = by.into_iter().collect();
    rows.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    let total: usize = live.iter().map(|e| e.size).sum();
    eprintln!("[alloc] pid {}: {} live allocations, {} MiB", std::process::id(), live.len(), total >> 20);
    for (frames, (count, bytes, ptrs)) in rows.into_iter().take(25) {
        let names: Vec<String> = frames
            .iter()
            .filter(|&&f| f != 0)
            .map(|&f| omni_platform::sampler::symbolize(f).map_or_else(|| format!("{f:#x}"), |(n, _)| n))
            .filter(|n| !n.starts_with("omni_linux::alloc_trace") && !n.starts_with("alloc::") && !n.starts_with("__rust") && !n.contains("raw_vec"))
            .take(6)
            .collect();
        let at: Vec<String> = ptrs.iter().map(|p| format!("{p:#x}")).collect();
        eprintln!("[alloc]   {count} x {} KiB = {} MiB at {}: {}", bytes / count >> 10, bytes >> 20, at.join(" "), names.join(" < "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracked_blocks_are_found_and_removed() {
        // Directly on the table: the allocator itself is not this test binary's.
        for p in (1..200usize).map(|i| i * 0x10_0000) {
            track(p, 1 << 20);
        }
        let n = |p: usize| TABLE.lock().iter().filter(|e| e.ptr == p).count();
        assert_eq!(n(0x50_0000), 1);
        for p in (1..200usize).step_by(2).map(|i| i * 0x10_0000) {
            untrack(p);
        }
        assert_eq!(n(0x30_0000), 0);
        assert_eq!(n(0x40_0000), 1, "the others stay findable after the deletions");
        for p in (2..200usize).step_by(2).map(|i| i * 0x10_0000) {
            untrack(p);
        }
        assert!(TABLE.lock().iter().all(|e| e.ptr == 0));
    }
}
