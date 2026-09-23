//! **D10 and D12, re-measured on the Linux port host.** Probes, not budgets: each prints a figure
//! with its `n` and method, and asserts only what the design depends on -- that a reservation is
//! free, that decommit returns what commit took, that bulk commit beats per-page faulting, that the
//! dual-mapped arena has no mismatches -- so that a figure that flips one of those fails loudly.
//!
//! Every timing is the **median of n runs** of the operation shown, in a release build, on the
//! i5-4460 port host with the owner's desktop session live (so: not a quiet machine). They are
//! recorded in `docs/ports/linux-notes/mem.md`.
//!
//! One `#[test]`, so that nothing else in this binary allocates or faults while it measures.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::sync::Arc;
use std::time::Instant;

use omni_mem::{
    CodeArena, CommitPolicy, DemandPager, GuestSpace, GuestSpaceConfig, Placement, Protection,
};
use omni_platform::vm;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const GIB: usize = 1024 * MIB;
const TIB: usize = 1024 * GIB;

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn charge() -> i64 {
    vm::process_commit_charge().expect("commit charge") as i64
}

fn resident() -> i64 {
    vm::process_working_set().expect("working set") as i64
}

/// Reservation: time per call and what it costs, at 1 GiB to 64 TiB; the largest single one.
fn reservation() {
    println!("\n== reservation (PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS) ==");
    const N: usize = 31;
    for size in [GIB, 16 * GIB, TIB, 16 * TIB, 64 * TIB] {
        let mut times = Vec::with_capacity(N);
        let mut worst_charge = 0i64;
        let mut worst_resident = 0i64;
        for _ in 0..N {
            let (c, r) = (charge(), resident());
            let started = Instant::now();
            let reservation = vm::reserve(size, vm::page_size()).expect("reserve");
            times.push(started.elapsed().as_nanos() as f64);
            worst_charge = worst_charge.max((charge() - c).abs());
            worst_resident = worst_resident.max(resident() - r);
            vm::release(reservation).expect("release");
        }
        println!(
            "reserve {:>7} GiB: {:>8.0} ns/call (median, n = {N}); commit charge moved at most \
             {worst_charge} B, resident at most {worst_resident} B",
            size / GIB,
            median(times)
        );
        assert_eq!(worst_charge, 0, "a reservation of {size} bytes was charged");
    }
    // The largest single reservation: bisect between 64 TiB (just shown to fit) and 128 TiB (the
    // whole 47-bit user half, which cannot fit beside everything else). It depends on where ASLR put
    // this process's executable and libraries -- the largest hole is what is left -- so it is a
    // figure for this process, not a constant.
    let (mut good, mut bad) = (64 * TIB, 128 * TIB);
    while bad - good > 64 * GIB {
        let middle = good + (bad - good) / 2;
        match vm::reserve(middle, vm::page_size()) {
            Ok(reservation) => {
                vm::release(reservation).expect("release");
                good = middle;
            }
            Err(_) => bad = middle,
        }
    }
    println!(
        "largest single reservation: {:.2} TiB (bisected to 64 GiB)",
        good as f64 / TIB as f64
    );

    // 64 spaces of 16 GiB: 1 TiB of guest address space.
    let before = charge();
    let spaces: Vec<_> = (0..64)
        .map(|_| {
            GuestSpace::with_config(GuestSpaceConfig { size: 16 * GIB, ..GuestSpaceConfig::default() })
                .expect("a 16 GiB guest space")
        })
        .collect();
    let cost = charge() - before;
    println!("64 guest spaces x 16 GiB (1 TiB): commit charge {cost} B");
    assert!(cost < MIB as i64, "64 reservations cost {cost} B");
    drop(spaces);
}

/// Commit and decommit, per page and in bulk.
fn commit_and_decommit() {
    println!("\n== commit / decommit (plain reservation: mprotect / mmap(MAP_FIXED, PROT_NONE)) ==");
    let page = vm::page_size();
    const PAGES: usize = 16 * 1024; // 64 MiB
    const N: usize = 11;
    let mut per_page_commit = Vec::new();
    let mut per_page_decommit = Vec::new();
    let mut bulk_commit = Vec::new();
    let mut bulk_decommit = Vec::new();
    let mut soft_fault = Vec::new();
    for _ in 0..N {
        let reservation = vm::reserve(PAGES * page, page).expect("reserve");
        let base = reservation.as_ptr();
        // One call per page, on alternate pages so that no two neighbours merge into one VMA
        // between calls (a merge is a cheaper call than the worst case).
        let started = Instant::now();
        for index in (0..PAGES).step_by(2) {
            // SAFETY: inside the live reservation.
            unsafe { vm::commit(base.add(index * page), page, Protection::ReadWrite) }.expect("commit");
        }
        per_page_commit.push(started.elapsed().as_nanos() as f64 / (PAGES / 2) as f64);
        let started = Instant::now();
        for index in (0..PAGES).step_by(2) {
            // SAFETY: committed above.
            unsafe { vm::decommit(base.add(index * page), page) }.expect("decommit");
        }
        per_page_decommit.push(started.elapsed().as_nanos() as f64 / (PAGES / 2) as f64);

        let started = Instant::now();
        // SAFETY: the whole reservation.
        unsafe { vm::commit(base, PAGES * page, Protection::ReadWrite) }.expect("bulk commit");
        bulk_commit.push(started.elapsed().as_nanos() as f64 / PAGES as f64);
        // First touch of committed pages: the kernel's own soft fault, no handler involved.
        let started = Instant::now();
        for index in 0..PAGES {
            // SAFETY: committed read-write.
            unsafe { core::ptr::write_volatile(base.add(index * page), 1) };
        }
        soft_fault.push(started.elapsed().as_nanos() as f64 / PAGES as f64);
        let started = Instant::now();
        // SAFETY: committed and touched; nothing refers to it any more.
        unsafe { vm::decommit(base, PAGES * page) }.expect("bulk decommit");
        bulk_decommit.push(started.elapsed().as_nanos() as f64 / PAGES as f64);
        vm::release(reservation).expect("release");
    }
    println!(
        "commit one page per call      {:>7.1} ns/page (median of n = {N} runs of {} calls)",
        median(per_page_commit),
        PAGES / 2
    );
    println!(
        "decommit one page per call    {:>7.1} ns/page (median of n = {N} runs of {} calls)",
        median(per_page_decommit),
        PAGES / 2
    );
    println!("commit 64 MiB in one call     {:>7.1} ns/page (median of n = {N})", median(bulk_commit));
    println!(
        "decommit 64 MiB (touched)     {:>7.1} ns/page (median of n = {N})",
        median(bulk_decommit)
    );
    println!(
        "kernel soft fault, first touch {:>6.1} ns/page (median of n = {N} runs of {PAGES} pages)",
        median(soft_fault.clone())
    );
    SOFT_FAULT.with(|cell| cell.set(median(soft_fault)));
}

std::thread_local! {
    static SOFT_FAULT: core::cell::Cell<f64> = const { core::cell::Cell::new(0.0) };
}

/// The demand pager's cost per fault, against the kernel soft fault and against bulk commit.
fn demand_paging() {
    println!("\n== demand paging: SIGSEGV -> omni-platform -> DemandPager -> ensure_committed ==");
    const N: usize = 11;
    for (granule, pages) in [(4 * KIB, 8 * 1024usize), (64 * KIB, 16 * 1024)] {
        let mut per_fault = Vec::new();
        let mut per_page = Vec::new();
        let mut faults = 0;
        for _ in 0..N {
            let space = Arc::new(
                GuestSpace::with_config(GuestSpaceConfig {
                    size: 256 * MIB,
                    commit_granule: granule,
                    ..GuestSpaceConfig::default()
                })
                .expect("a guest space"),
            );
            let pager = DemandPager::install(Arc::clone(&space)).expect("a pager");
            let page = space.page_size();
            let lazy = space
                .map_anonymous(
                    Placement::Anywhere { align: 64 * KIB },
                    pages * page,
                    Protection::ReadWrite,
                    CommitPolicy::Lazy,
                )
                .expect("a lazy mapping");
            let before = pager.stats().resolved;
            let started = Instant::now();
            for index in 0..pages {
                // SAFETY: inside the lazy mapping; the pager commits it on first touch.
                unsafe { core::ptr::write_volatile((lazy + index * page) as *mut u8, 1) };
            }
            let elapsed = started.elapsed().as_nanos() as f64;
            faults = pager.stats().resolved - before;
            per_fault.push(elapsed / faults as f64);
            per_page.push(elapsed / pages as f64);
            drop(pager);
            space.unmap(lazy, pages * page).expect("unmap");
        }
        println!(
            "granule {:>3} KiB: {:>7.0} ns per pager fault, {:>6.1} ns per page touched ({faults} \
             faults per run over {} pages; median of n = {N} runs)",
            granule / KIB,
            median(per_fault),
            median(per_page),
            pages
        );
    }
    println!(
        "(the kernel soft fault measured above is {:.1} ns/page)",
        SOFT_FAULT.with(core::cell::Cell::get)
    );
}

/// D12: emit and execute through the dual-mapped arena, against flipping one mapping with
/// `mprotect`. The same 6-byte function (`mov eax, imm32; ret`) with a new constant each cycle, and
/// every return value checked, so a stale instruction stream is a counted mismatch.
fn emit_and_execute() {
    println!("\n== D12: emit + execute ==");
    const TRIALS: usize = 200_000;
    const N: usize = 5;
    fn code(value: u32) -> [u8; 6] {
        let mut code = [0xB8, 0, 0, 0, 0, 0xC3];
        code[1..5].copy_from_slice(&value.to_le_bytes());
        code
    }

    let arena = CodeArena::new().expect("an arena");
    let block = arena.alloc(64).expect("a block");
    let mut dual = Vec::new();
    let mut mismatches = 0u64;
    for _ in 0..N {
        let started = Instant::now();
        for trial in 0..TRIALS {
            let value = trial as u32 ^ 0x5A5A_0000;
            arena.write(&block, 0, &code(value)).expect("emit");
            // SAFETY: the block holds a complete `mov eax, imm32; ret` just written through the
            // writable view of the same pages.
            let function: extern "C" fn() -> u32 = unsafe { core::mem::transmute(block.exec_ptr()) };
            if function() != value {
                mismatches += 1;
            }
        }
        dual.push(started.elapsed().as_nanos() as f64 / TRIALS as f64);
    }

    let page = vm::page_size();
    let reservation = vm::reserve(page, page).expect("reserve a page");
    let base = reservation.as_ptr();
    // SAFETY: the page is this test's own.
    unsafe { vm::commit(base, page, Protection::ReadWrite) }.expect("commit");
    let mut flipping = Vec::new();
    let mut flip_mismatches = 0u64;
    for _ in 0..N {
        let started = Instant::now();
        for trial in 0..TRIALS {
            let value = trial as u32 ^ 0xA5A5_0000;
            // SAFETY: the page is read-write here, executable below, and never both.
            unsafe {
                core::ptr::copy_nonoverlapping(code(value).as_ptr(), base, 6);
                vm::protect(base, page, Protection::ReadExecute).expect("to r-x");
                let function: extern "C" fn() -> u32 = core::mem::transmute(base);
                if function() != value {
                    flip_mismatches += 1;
                }
                vm::protect(base, page, Protection::ReadWrite).expect("back to rw-");
            }
        }
        flipping.push(started.elapsed().as_nanos() as f64 / TRIALS as f64);
    }
    vm::release(reservation).expect("release");
    let (dual, flipping) = (median(dual), median(flipping));
    println!(
        "dual-mapped (memfd, RW + RX views): {dual:>7.1} ns per emit+execute, {mismatches} \
         mismatches in {} trials (median of n = {N} runs of {TRIALS})",
        N * TRIALS
    );
    println!(
        "mprotect RW -> RX -> RW, one page: {flipping:>7.1} ns per emit+execute, {flip_mismatches} \
         mismatches in {} trials (median of n = {N} runs of {TRIALS}); {:.1}x the dual mapping",
        N * TRIALS,
        flipping / dual
    );
    assert_eq!(mismatches, 0, "the executable view showed a stale instruction stream");
    assert_eq!(flip_mismatches, 0);
    assert!(dual < flipping, "the dual mapping must be the faster path, or D12's choice is wrong here");
}

#[test]
fn d10_and_d12_re_measured_on_this_host() {
    if cfg!(debug_assertions) {
        println!("note: a debug build; the figures in the port notes come from --release");
    }
    reservation();
    commit_and_decommit();
    demand_paging();
    emit_and_execute();
}
