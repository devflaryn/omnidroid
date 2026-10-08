//! `GuestSpace::reset_zero_pages`: the guest's resident pages of zeros leave the host process's
//! working set, and **no byte the guest reads changes** -- not the zeros, not the pages around
//! them, not a write made while the sweep runs. Windows only (elsewhere it does nothing).
#![cfg(target_os = "windows")]

mod common;

use common::MIB;
use omni_mem::{CommitPolicy, GuestSpace, GuestSpaceConfig, Placement, Protection, SMALL_PAGE};

const PAGE: usize = SMALL_PAGE;

fn space() -> GuestSpace {
    GuestSpace::with_config(GuestSpaceConfig { size: 64 * MIB, ..GuestSpaceConfig::default() }).expect("reserve")
}

/// A lazily committed read-write mapping of `pages` pages, every page touched: page `i` holds
/// `i` at its first byte when `i % 3 == 0`, and is all zeros (written as zeros, as a guest's
/// `memset` writes them) otherwise.
fn touched(space: &GuestSpace, pages: usize) -> usize {
    let at = space
        .map_anonymous(Placement::Anywhere { align: space.page_size() }, pages * PAGE, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("map");
    space.ensure_committed(at, pages * PAGE).expect("commit");
    for i in 0..pages {
        let p = (space.host_addr(at) + i * PAGE) as *mut u8;
        // SAFETY: committed and writable just above.
        unsafe { p.write_volatile(if i % 3 == 0 { i as u8 | 1 } else { 0 }) };
    }
    at
}

#[test]
fn zero_pages_leave_the_working_set_and_every_byte_reads_as_before() {
    let space = space();
    const PAGES: usize = 300;
    let at = touched(&space, PAGES);
    let nonzero = (0..PAGES).filter(|i| i % 3 == 0).count();

    let first = space.reset_zero_pages().expect("sweep");
    assert_eq!(first.committed, PAGES, "{first:?}");
    assert_eq!(first.resident, PAGES, "every page was touched: {first:?}");
    assert_eq!(first.zero, PAGES - nonzero, "{first:?}");
    assert_eq!(first.reset, PAGES - nonzero, "nothing else writes, so every zero page goes: {first:?}");

    // A second sweep finds only the pages that hold something.
    let second = space.reset_zero_pages().expect("sweep");
    assert_eq!((second.resident, second.zero), (nonzero, 0), "{second:?}");

    for i in 0..PAGES {
        let p = (space.host_addr(at) + i * PAGE) as *const u8;
        // SAFETY: still committed: the sweep never decommits.
        let first_byte = unsafe { p.read_volatile() };
        assert_eq!(first_byte, if i % 3 == 0 { i as u8 | 1 } else { 0 }, "page {i}");
    }
    // And a reset page takes a write like any other.
    let p = (space.host_addr(at) + PAGE + 17) as *mut u8;
    // SAFETY: committed and writable.
    unsafe { p.write_volatile(0x5A) };
    // SAFETY: as above.
    assert_eq!(unsafe { p.read_volatile() }, 0x5A);
}

#[test]
fn file_views_none_pages_and_untouched_pages_are_left_alone() {
    let space = space();
    let lazy = space
        .map_anonymous(Placement::Anywhere { align: space.page_size() }, 64 * PAGE, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("map");
    // Committed but never touched: not resident, so nothing to do.
    space.ensure_committed(lazy, 16 * PAGE).expect("commit");
    // A PROT_NONE mapping is never read.
    let none = space
        .map_anonymous(Placement::Anywhere { align: space.page_size() }, 16 * PAGE, Protection::None, CommitPolicy::Lazy)
        .expect("map");
    let _ = none;
    let swept = space.reset_zero_pages().expect("sweep");
    assert_eq!((swept.resident, swept.reset), (0, 0), "{swept:?}");
}

/// A guest thread writes while the sweep runs, over and over: every value written is read back.
#[test]
fn writes_racing_the_sweep_are_kept() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    const PAGES: usize = 2048;
    let space = Arc::new(space());
    let at = space
        .map_anonymous(Placement::Anywhere { align: space.page_size() }, PAGES * PAGE, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("map");
    space.ensure_committed(at, PAGES * PAGE).expect("commit");
    let base = space.host_addr(at);
    for i in 0..PAGES {
        // SAFETY: committed: a zero written makes each page resident.
        unsafe { ((base + i * PAGE) as *mut u64).write_volatile(0) };
    }
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut n = 0usize;
            while !stop.load(Ordering::Relaxed) && n < PAGES * 4 {
                // Slot n / PAGES of page n % PAGES, each written once, never with zero: a page
                // reads zero until its first write, and the first round of writes races the
                // sweeps -- slowly enough to span many of them.
                // SAFETY: inside the committed mapping; the sweep never decommits.
                unsafe { ((base + (n % PAGES) * PAGE + (n / PAGES) * 8) as *mut u64).write_volatile(n as u64 + 1) };
                n += 1;
                if n % 16 == 0 {
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
            }
            n
        })
    };
    let mut total = omni_mem::ZeroPages::default();
    let until = std::time::Instant::now() + std::time::Duration::from_millis(400);
    while std::time::Instant::now() < until {
        total = total.add(space.reset_zero_pages().expect("sweep"));
    }
    stop.store(true, Ordering::Relaxed);
    let n = writer.join().expect("writer");
    eprintln!("{n} writes raced {total:?}");
    assert!(total.reset > 0, "no page was ever reset: the race was not run ({total:?})");
    for k in 0..n {
        // SAFETY: committed.
        let v = unsafe { ((base + (k % PAGES) * PAGE + (k / PAGES) * 8) as *const u64).read_volatile() };
        assert_eq!(v, k as u64 + 1, "write {k} of {n} lost ({total:?})");
    }
}
