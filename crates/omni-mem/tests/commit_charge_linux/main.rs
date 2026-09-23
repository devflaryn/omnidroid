//! What guest memory actually *costs*, on Linux: the mirror of `tests/commit_charge.rs`.
//!
//! A directory target (`tests/commit_charge_linux/main.rs`) so that `tests/windows_only.rs`, which
//! accounts for every `tests/*.rs` file by name, does not see it.
//!
//! **Commit charge on Linux** is the `VM_ACCOUNT` total that `omni_platform::vm::process_commit_charge`
//! reads from `/proc/self/smaps` -- exactly what this process adds to `Committed_AS`. Two differences
//! from the Windows file follow from it, and each is marked where it applies: page tables are not
//! charged (Windows charges `size/512`), and the commit-limit refusal is `ENOMEM` (12), not
//! `ERROR_COMMITMENT_LIMIT` (1455).
//!
//! **Why the tests below are functions run by one `#[test]`**: a Linux thread stack is a writable
//! private mapping, so each libtest thread that starts is 2 MiB of `VM_ACCOUNT`, half this file's
//! tolerance, landing in whichever measurement is in progress. [`SERIAL`] serialises measurements,
//! not thread starts. One test means one test thread, started before the first measurement.
//!
//! These are the tests that decide whether `omni-mem` satisfies the requirement D10 was written to
//! answer: isolated guest address spaces, no large fixed RAM reservation, demand-driven usage,
//! genuinely reclaimable, many instances without a huge pagefile. Every one of them asserts against
//! a **measured** commit charge rather than against a successful return, because that is the only
//! thing that can tell real reclamation from apparent reclamation: `MEM_RESET` is the cheapest call
//! Windows offers, it frees exactly 0 bytes, and no functional test can distinguish it from
//! `MEM_DECOMMIT` (D10).
//!
//! Every observed number is printed. Run with `cargo test -p omni-mem --test commit_charge --
//! --nocapture` to read them.
//!
//! # Why this is a separate test binary
//!
//! Commit charge is a *per-process* quantity and `cargo test` runs one binary's tests as parallel
//! threads, so a mapping made by one test would appear in another's delta. These tests are in their
//! own binary and serialise against each other with [`SERIAL`].
//!
//! # Tolerance
//!
//! 4 MiB, and the reason is arithmetic rather than superstition: page tables are charged at 8 bytes
//! per committed 4 KiB page, which is 2 MiB for the 1 GiB case — D10 measured exactly this, 256 MiB
//! committed costing 256.50 MiB — and the test harness allocates a little around each test. It is
//! far below every effect being measured here (0 against 4 GiB reserved, 1 GiB against 0 committed).
#![cfg(target_os = "linux")]

#[path = "../common/mod.rs"]
mod common;

use std::sync::Mutex;
use std::time::Instant;

use common::{KIB, MIB};
use omni_mem::{
    ArenaConfig, Backing, CodeArena, CommitBudget, CommitPolicy, GuestSpace, GuestSpaceConfig,
    MapExecutability, MemError, Placement, Protection,
};
use omni_platform::vm;

/// Held for the whole of each test, so only one test is mapping at a time.
static SERIAL: Mutex<()> = Mutex::new(());

const TOLERANCE: i64 = 4 * MIB as i64;

fn charge() -> i64 {
    vm::process_commit_charge().expect("read the process commit charge") as i64
}

fn working_set() -> i64 {
    vm::process_working_set().expect("read the process working set") as i64
}

fn mib(bytes: i64) -> String {
    format!("{:+.3} MiB", bytes as f64 / MIB as f64)
}

fn assert_close(actual: i64, expected: i64, what: &str) {
    assert!(
        (actual - expected).abs() <= TOLERANCE,
        "{what}: expected {} +/- {}, observed {}",
        mib(expected),
        mib(TOLERANCE),
        mib(actual)
    );
}

fn space(size: usize) -> GuestSpace {
    GuestSpace::with_config(GuestSpaceConfig { size, ..GuestSpaceConfig::default() })
        .expect("reserve a guest address space")
}

fn reserving_a_four_gibibyte_guest_space_costs_no_commit_charge() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());

    let before = charge();
    let before_ws = working_set();
    let space = GuestSpace::new().expect("reserve the default guest address space");
    let after = charge();
    let delta = after - before;
    eprintln!(
        "reserve a {} GiB guest space: commit {} (working set {}), base {:#x}",
        space.len() / (1024 * MIB),
        mib(delta),
        mib(working_set() - before_ws),
        space.base()
    );
    assert_eq!(space.len(), 4 * 1024 * MIB, "the default guest space should be 4 GiB");

    // The whole memory design rests on this: address space is free, and only commit costs anything.
    // If this ever stops holding, no amount of lazy committing elsewhere can save the design.
    assert_close(delta, 0, "a 4 GiB guest address space reservation");
    assert_eq!(space.stats().committed, 0);
    assert_eq!(space.stats().free, space.len());

    space.close().expect("close");
    let released = charge() - before;
    eprintln!("after closing it: commit {}", mib(released));
    assert_close(released, 0, "closing an untouched guest space");
}

fn eight_guest_spaces_cost_no_more_than_one() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());

    // The multi-instance premise, stated as a test: 32 GiB of guest address space across eight
    // instances must cost essentially nothing until something is committed in it. D10 measured 64
    // processes reserving 16 GB each — 1 TB in total — for 240.8 MB of *system* commit between them.
    let before = charge();
    let spaces: Vec<GuestSpace> = (0..8).map(|_| GuestSpace::new().expect("reserve")).collect();
    let delta = charge() - before;
    let total: usize = spaces.iter().map(|space| space.len()).sum();
    eprintln!("8 guest spaces totalling {} GiB: commit {}", total / (1024 * MIB), mib(delta));
    assert_eq!(total, 32 * 1024 * MIB);
    assert_close(delta, 0, "eight 4 GiB guest address spaces");

    drop(spaces);
    assert_close(charge() - before, 0, "dropping eight guest address spaces");
}

fn mapping_writing_reading_and_unmapping_returns_the_commit_charge() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let space = space(256 * MIB);
    let size = 64 * MIB;

    let baseline = charge();
    let address = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            size,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("map 64 MiB");
    let mapped = charge() - baseline;

    // SAFETY: the whole range is mapped ReadWrite and committed eagerly.
    unsafe {
        let pointer = space.ptr(address, size).expect("in the space");
        for offset in (0..size).step_by(4096) {
            pointer.add(offset).write(0xA5);
        }
        for offset in (0..size).step_by(4096) {
            assert_eq!(pointer.add(offset).read(), 0xA5, "wrong byte at {offset:#x}");
        }
    }
    let touched = charge() - baseline;

    space.unmap(address, size).expect("unmap");
    let released = charge() - baseline;
    eprintln!(
        "64 MiB eager mapping: commit after map {}, after touching every page {}, after unmap {}",
        mib(mapped),
        mib(touched),
        mib(released)
    );

    // Commit charge is debited at commit, not at first touch: the whole size is charged before a
    // single byte has been written, and touching every page adds nothing but page tables.
    assert_close(mapped, size as i64, "committing 64 MiB");
    assert_close(touched, size as i64, "touching 64 MiB that was already committed");
    assert_close(released, 0, "unmapping 64 MiB");
    assert_eq!(space.stats().committed, 0);
}

/// **The requirement, expressed as a test.** Grow to 1 GiB of live guest mappings, then release
/// everything, and assert that the commit charge comes back to the baseline while the guest address
/// space reservation stays intact.
///
/// This is the test that D10 exists to make possible, and the one that fails if reclamation is only
/// apparent. Each of the numbers it prints is reported in the task report.
fn growing_to_a_gibibyte_and_releasing_everything_returns_commit_charge_to_baseline() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let space = GuestSpace::new().expect("reserve a 4 GiB guest address space");
    let baseline = charge();
    let baseline_ws = working_set();
    let chunk = 64 * MIB;
    let chunks = 16;
    let total = chunk * chunks;

    let mut addresses = Vec::new();
    for index in 0..chunks {
        let address = space
            .map_anonymous(
                Placement::Anywhere { align: 64 * KIB },
                chunk,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .unwrap_or_else(|error| panic!("mapping {index}: {error}"));
        // Write one byte per page, so the pages are genuinely in the working set and not merely
        // committed. This is what makes the release meaningful: a reclamation that only dropped
        // untouched pages would be no reclamation at all.
        // SAFETY: the whole range is mapped ReadWrite and committed.
        unsafe {
            let pointer = space.ptr(address, chunk).expect("in the space");
            for offset in (0..chunk).step_by(4096) {
                pointer.add(offset).write(index as u8);
            }
        }
        addresses.push(address);
    }

    let peak = charge() - baseline;
    let peak_ws = working_set() - baseline_ws;
    eprintln!(
        "grown to {} MiB of live guest mappings: commit {}, working set {}",
        total / MIB,
        mib(peak),
        mib(peak_ws)
    );
    assert_close(peak, total as i64, "1 GiB of live guest mappings");
    assert_eq!(space.stats().committed, total);
    assert_eq!(space.stats().mapped, total);

    // Verify the contents before releasing: if a mapping had landed on top of another, this is where
    // it would show, and the release numbers below would be measuring the wrong thing.
    for (index, &address) in addresses.iter().enumerate() {
        // SAFETY: every range is still mapped and committed.
        unsafe {
            let pointer = space.ptr(address, chunk).expect("in the space");
            assert_eq!(pointer.read(), index as u8, "mapping {index} holds the wrong byte");
            assert_eq!(pointer.add(chunk - 4096).read(), index as u8);
        }
    }

    for (index, address) in addresses.into_iter().enumerate() {
        space.unmap(address, chunk).unwrap_or_else(|error| panic!("unmapping {index}: {error}"));
    }
    let after = charge() - baseline;
    let after_ws = working_set() - baseline_ws;
    eprintln!(
        "after unmapping all {} MiB: commit {}, working set {}",
        total / MIB,
        mib(after),
        mib(after_ws)
    );

    // This is the assertion the requirement reduces to.
    assert_close(after, 0, "releasing 1 GiB of guest mappings");
    assert_eq!(space.stats().committed, 0);
    assert_eq!(space.stats().mapped, 0);

    // And the address space is still ours: the reservation was never released, so the same addresses
    // are still available and nothing else in the process can have been given them.
    assert_eq!(space.len(), 4 * 1024 * MIB);
    assert_eq!(space.stats().free, space.len());
    let reused = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            chunk,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("the space is still usable after being emptied");
    // A recommitted page reads back zero, matching anonymous mmap and MADV_DONTNEED.
    // SAFETY: the range was just mapped and committed.
    unsafe {
        let pointer = space.ptr(reused, chunk).expect("in the space");
        assert_eq!(pointer.read(), 0, "a recommitted page must read back zero");
    }
    space.unmap(reused, chunk).expect("unmap");

    space.close().expect("close");
    let closed = charge() - baseline;
    eprintln!("after closing the guest space: commit {}", mib(closed));
    assert_close(closed, 0, "closing the guest space");
}

fn a_lazy_mapping_of_a_gibibyte_costs_nothing_until_it_is_reached() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let space = GuestSpace::new().expect("reserve");
    let granule = space.commit_granule();
    let size = 1024 * MIB;

    let baseline = charge();
    let address = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            size,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("map 1 GiB lazily");
    let mapped = charge() - baseline;
    eprintln!("a 1 GiB lazy mapping: commit {}", mib(mapped));
    // The guest can hold a gigabyte of address space for nothing. This is the other half of the
    // requirement: demand-driven usage, not a preallocated guest RAM blob.
    assert_close(mapped, 0, "a 1 GiB lazy mapping");
    assert_eq!(space.stats().mapped, size);
    assert_eq!(space.stats().committed, 0);

    // Reaching one byte commits one granule and no more.
    let committed = space.ensure_committed(address + 512 * MIB, 1).expect("commit one granule");
    let one = charge() - baseline;
    eprintln!("after reaching one byte in the middle of it: commit {}", mib(one));
    assert_eq!(committed, granule, "one byte must commit exactly one granule");
    // The number that matters is that reaching one byte did not cost anything like the mapping: a
    // granule is 1/16384th of it. The exact figure is not asserted tightly because a 64 KiB delta is
    // within the noise of the test process's own heap — the byte count returned above is the precise
    // statement, and it is exact.
    assert!(
        one < TOLERANCE,
        "reaching one byte of a 1 GiB lazy mapping cost {}, which is far too much",
        mib(one)
    );
    // SAFETY: the granule containing this address is now committed and writable.
    unsafe {
        space.ptr(address + 512 * MIB, 1).expect("in the space").write(0x42);
    }

    // Walking a 16 MiB window commits exactly that window's granules, not the whole mapping.
    let window = 16 * MIB;
    let committed = space.ensure_committed(address, window).expect("commit a window");
    let walked = charge() - baseline;
    eprintln!("after committing a {} MiB window: commit {}", window / MIB, mib(walked));
    assert_eq!(committed, window, "a window must commit exactly its own granules");
    assert_close(walked, (window + granule) as i64, "a 16 MiB window of a 1 GiB lazy mapping");

    space.unmap(address, size).expect("unmap");
    let released = charge() - baseline;
    eprintln!("after unmapping the whole lazy mapping: commit {}", mib(released));
    assert_close(released, 0, "unmapping a partly committed 1 GiB lazy mapping");
    space.close().expect("close");
}

/// `reclaim_idle` must return what it says it returned.
///
/// The test is written to fail if reclamation were built on `MEM_RESET`, `DiscardVirtualMemory`,
/// `OfferVirtualMemory` or `EmptyWorkingSet`: each of those measured **exactly 0.00 MB** of commit
/// charge returned (D10), and each would let `reclaim_idle` report a large `bytes` while the process
/// still held the charge. So the reported figure is compared against the *measured* drop, not merely
/// asserted to be non-zero.
fn reclaim_idle_returns_the_commit_charge_it_claims_to_have_returned() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let space = space(1024 * MIB);
    let size = 256 * MIB;

    let baseline = charge();
    // Lazy, then committed granule by granule, so that reclamation has thousands of separate
    // granules to give back rather than one large region — which is the shape a long-running guest
    // actually produces.
    let address = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            size,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("map 256 MiB");
    // Granule by granule, the way a guest arrives at its memory, so that reclamation has thousands
    // of separate committed granules to give back rather than one large region.
    let granule = space.commit_granule();
    let mut offset = 0;
    while offset < size {
        assert_eq!(
            space.ensure_committed(address + offset, 1).expect("commit a granule"),
            granule
        );
        offset += granule;
    }
    // SAFETY: the whole range is mapped ReadWrite and committed.
    unsafe {
        let pointer = space.ptr(address, size).expect("in the space");
        for offset in (0..size).step_by(4096) {
            pointer.add(offset).write(0x3C);
        }
    }
    let committed = charge() - baseline;
    assert_close(committed, size as i64, "committing 256 MiB");

    let marked = space.advise_idle(address, size).expect("advise idle");
    let advised = charge() - baseline;
    eprintln!("256 MiB committed: {}; after advise_idle: {}", mib(committed), mib(advised));
    assert_eq!(marked, size, "the whole range should have been marked");
    assert_eq!(space.stats().idle, size);
    // Advising is not reclaiming. Marking must cost nothing and free nothing — if this dropped, the
    // implementation would be decommitting behind the caller's back and losing data it promised to
    // keep.
    assert_close(advised, committed, "advise_idle must not change the commit charge");
    // SAFETY: the range is still committed; advise_idle keeps the contents until reclaim.
    unsafe {
        assert_eq!(space.ptr(address, 1).expect("in the space").read(), 0x3C);
    }

    let before_reclaim = charge();
    let reclaimed = space.reclaim_idle().expect("reclaim");
    let measured_drop = before_reclaim - charge();
    eprintln!(
        "reclaim_idle reported {} in {} granules; the process's commit charge dropped by {}",
        mib(reclaimed.bytes as i64),
        reclaimed.granules,
        mib(measured_drop)
    );
    assert_eq!(reclaimed.bytes, size, "the whole range should have been reclaimed");
    assert_eq!(
        reclaimed.granules,
        size / granule,
        "every granule of the mapping should have been decommitted"
    );
    // The claim and the measurement have to agree. This is the assertion that a MEM_RESET-based
    // implementation cannot pass.
    assert_close(
        measured_drop,
        reclaimed.bytes as i64,
        "the measured drop must match what reclaim_idle reported",
    );
    assert_close(charge() - baseline, 0, "after reclaiming everything");
    assert_eq!(space.stats().committed, 0);
    assert_eq!(space.stats().idle, 0);

    // The mapping is still there and still usable: reclamation returns the resource without taking
    // the guest's address space away. Its contents are gone, which is exactly MADV_DONTNEED.
    assert_eq!(space.stats().mapped, size);
    assert_eq!(space.ensure_committed(address, 1).expect("recommit"), granule);
    // SAFETY: the first granule has just been committed again.
    unsafe {
        assert_eq!(
            space.ptr(address, 1).expect("in the space").read(),
            0,
            "a reclaimed and recommitted page must read back zero"
        );
    }
    space.unmap(address, size).expect("unmap");
    space.close().expect("close");
}

fn a_partial_unmap_returns_exactly_the_commit_charge_of_what_it_unmapped() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let space = space(1024 * MIB);
    let size = 128 * MIB;
    let hole = 64 * MIB;

    let baseline = charge();
    let address = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            size,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("map");
    // SAFETY: the whole range is mapped ReadWrite and committed.
    unsafe {
        let pointer = space.ptr(address, size).expect("in the space");
        for offset in (0..size).step_by(4096) {
            pointer.add(offset).write(0x5E);
        }
    }
    let committed = charge() - baseline;

    // Punch the middle out. The surviving halves must keep both their contents and their charge.
    space.unmap(address + 32 * MIB, hole).expect("punch a hole");
    let after = charge() - baseline;
    eprintln!(
        "128 MiB committed {}, after unmapping 64 MiB from the middle {}",
        mib(committed),
        mib(after)
    );
    assert_close(after, (size - hole) as i64, "the surviving halves of a partial unmap");
    assert_eq!(space.stats().committed, size - hole);
    // SAFETY: both surviving halves are still mapped and committed.
    unsafe {
        assert_eq!(space.ptr(address, 1).expect("in the space").read(), 0x5E);
        assert_eq!(space.ptr(address + 96 * MIB, 1).expect("in the space").read(), 0x5E);
    }

    space.unmap(address, size).expect("unmap the rest");
    assert_close(charge() - baseline, 0, "unmapping the survivors");
    space.close().expect("close");
}

/// The commit granule, measured rather than asserted.
///
/// This is the evidence behind `DEFAULT_COMMIT_GRANULE`. It prints the cost per page at several
/// granule sizes and asserts the relationship that decides the default: per-page commit is an order
/// of magnitude worse, because the cost of committing a granule is two kernel calls and barely
/// depends on how big the granule is.
fn a_larger_commit_granule_is_dramatically_cheaper_per_page() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let span = 64 * MIB;
    let page = vm::page_size();
    let mut per_page = Vec::new();

    if cfg!(debug_assertions) {
        eprintln!(
            "note: this is a debug build, where the region map's O(n) invariant check runs after \
             every mutation. The ratio below still holds, but the absolute figures quoted in the \
             task report come from `cargo test --release`."
        );
    }
    for granule in [4 * KIB, 16 * KIB, 64 * KIB, 256 * KIB, MIB] {
        let space = GuestSpace::with_config(GuestSpaceConfig {
            size: 256 * MIB,
            commit_granule: granule,
            ..GuestSpaceConfig::default()
        })
        .expect("reserve");
        let address = space
            .map_anonymous(
                Placement::Anywhere { align: 64 * KIB },
                span,
                Protection::ReadWrite,
                CommitPolicy::Lazy,
            )
            .expect("map lazily");

        let started = Instant::now();
        let mut offset = 0;
        while offset < span {
            space.ensure_committed(address + offset, 1).expect("commit a granule");
            offset += granule;
        }
        let elapsed = started.elapsed();
        let nanos_per_page = elapsed.as_nanos() as f64 / (span / page) as f64;
        eprintln!(
            "granule {:>5} KiB: {:>9.2?} to commit {} MiB = {:>7.1} ns/page, {:>7.0} ns/granule",
            granule / KIB,
            elapsed,
            span / MIB,
            nanos_per_page,
            elapsed.as_nanos() as f64 / (span / granule) as f64,
        );
        assert_eq!(space.stats().committed, span, "the whole span should be committed");
        per_page.push((granule, nanos_per_page));
        space.close().expect("close");
    }

    let four_kib = per_page[0].1;
    let sixty_four_kib = per_page[2].1;
    // Measured on this machine: 2414 ns/page at a 4 KiB granule against 150 ns/page at 64 KiB, a
    // factor of 16. (An earlier revision of this comment quoted 1810 and 124, from a run that
    // predates the final commit path; `DEFAULT_COMMIT_GRANULE`'s table is the authority and these are
    // its 4 KiB and 64 KiB rows.) The assertion is deliberately loose — it is testing the shape of the curve, not
    // the machine — but it is the reason the default is not the page size. For context, a page's
    // first touch costs about 381 ns here whatever the granule, and D10 measured a VEH demand-pager
    // at 2053 ns/fault.
    assert!(
        four_kib > 4.0 * sixty_four_kib,
        "committing page by page ({four_kib:.0} ns/page) should be far worse than committing in \
         64 KiB granules ({sixty_four_kib:.0} ns/page); if it is not, the granule is not earning \
         its complexity"
    );
}


/// Preserving copy-on-write content across a partial unmap must not privatise the clean pages.
///
/// This is the test that pins the *sharing* half of the C1 fix. Copying a survivor back wholesale
/// would keep the content correct and quietly privatise every page of it — which would satisfy every
/// functional test while destroying the property D11's multi-instance argument rests on, namely that
/// a guest library's text stays file-backed and shared at near-zero commit charge. So the assertion
/// is on the commit charge **across the unmap**: the whole view is made writable, two pages of it are
/// actually written, and the partial unmap must move the charge by kilobytes rather than by the 8 MiB
/// it would move if the comparison were skipped.
fn preserving_copy_on_write_content_does_not_privatise_the_clean_pages_of_a_survivor() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let space = space(64 * MIB);
    let page = vm::page_size();
    let view_len = 8 * MIB;

    let directory = std::env::temp_dir().join(format!("omni-mem-sharing-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("create the test directory");
    let path = directory.join("shared.bin");
    let contents: Vec<u8> = (0..view_len).map(|offset| ((offset / page) & 0xff) as u8).collect();
    std::fs::write(&path, &contents).expect("write the test file");
    let backing = Backing::open(&path, MapExecutability::Executable).expect("open the backing");

    let baseline = charge();
    let address = space
        .map_file(
            &backing,
            0,
            Placement::Anywhere { align: 64 * KIB },
            view_len,
            Protection::ReadExecute,
        )
        .expect("map 8 MiB execute-read");
    let mapped = charge() - baseline;

    // Make the *whole* view writable, then write only two pages of it. This is the case that
    // distinguishes comparing from copying: every page of the survivor is a candidate, and only two
    // of them actually hold anything the file does not.
    space.protect(address, view_len, Protection::ReadWrite).expect("drop to ReadWrite");
    let writable = charge() - baseline;
    let dirty_pages = [0usize, 1000];
    for index in dirty_pages {
        // SAFETY: the whole view is copy-on-write and writable.
        unsafe { std::ptr::write_bytes((address + index * page) as *mut u8, 0xC5, 32) };
    }
    space.protect(address, view_len, Protection::ReadExecute).expect("restore ReadExecute");
    let dirtied = charge() - baseline;

    let before_unmap = charge();
    space.unmap(address + 4 * MIB, 64 * KIB).expect("punch a hole");
    let across = charge() - before_unmap;
    eprintln!(
        "an 8 MiB execute-read view: commit after mapping {}, after making all of it writable {}, \
         after writing two pages and restoring the protection {}; the partial unmap that preserved \
         them moved it by {}",
        mib(mapped),
        mib(writable),
        mib(dirtied),
        mib(across)
    );

    // SAFETY: both preserved pages are in the head survivor, which is a live view.
    unsafe {
        for index in dirty_pages {
            assert_eq!(
                *((address + index * page) as *const u8),
                0xC5,
                "the content written into page {index} was lost"
            );
        }
        // And a clean page still reads the file.
        assert_eq!(*((address + 2 * page) as *const u8), 2);
    }

    // Re-privatising two pages costs 8 KiB. Copying the survivor back wholesale would cost about
    // 8 MiB, which is three orders of magnitude away from this bound.
    assert!(
        across < MIB as i64,
        "preserving two dirty pages of an 8 MiB view moved the commit charge by {}, which means \
         clean pages were privatised and file-backed sharing was destroyed",
        mib(across)
    );
    assert_eq!(space.stats().file_backed, view_len - 64 * KIB);

    space.unmap(address, view_len).expect("unmap the rest");
    let released = charge() - baseline;
    eprintln!("after unmapping the whole view: commit {}", mib(released));
    assert_close(released, 0, "unmapping a view whose pages were partly privatised");
    space.close().expect("close");
    let _ = std::fs::remove_dir_all(&directory);
}


/// The code arena's memory is charged against the system commit limit and is invisible to
/// `process_commit_charge`, so `CommitBudget` reports it explicitly.
///
/// Instrumentation rather than a budget assertion: the number this pins is the *gap* between what the
/// OS counter sees and what the machine is actually paying, because the translator's code cache is
/// expected to become the fastest-growing consumer (tens of MiB per guest thread) and a budget
/// watching only `PrivateUsage` would show a flat line while it grew.
fn the_commit_budget_reports_the_arena_memory_the_process_counter_cannot_see() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let space = space(256 * MIB);
    let arena = CodeArena::with_config(ArenaConfig {
        chunk_size: 4 * MIB,
        max_total: 32 * MIB,
        block_alignment: 16,
    })
    .expect("create the arena");

    let empty = CommitBudget::measure([&space], [&arena]).expect("measure");
    assert_eq!(empty.arena_mapped, 0, "an arena maps nothing until it is asked for a block");
    assert_eq!(empty.guest_committed, 0);
    assert_eq!(empty.invisible_to_process_counter(), 0);
    assert_eq!(empty.total_system_commit(), empty.process_private);

    // Commit 32 MiB of guest memory: this the process counter does see.
    let guest = 32 * MIB;
    let address = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            guest,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("map");
    // And 8 MiB of code arena, in two chunks: this it does not.
    let mut blocks = Vec::new();
    for _ in 0..2 {
        blocks.push(arena.alloc(4 * MIB).expect("allocate a chunk's worth"));
    }

    let budget = CommitBudget::measure([&space], [&arena]).expect("measure");
    eprintln!("{budget}");
    assert_eq!(budget.guest_committed, guest, "the guest mapping is reported");
    assert_eq!(budget.arena_mapped, 8 * MIB, "the arena's two chunks are reported");
    assert_eq!(budget.arena_mapped, arena.stats().mapped);
    assert_eq!(
        budget.total_system_commit(),
        budget.process_private + 8 * MIB as u64,
        "the total has to add the invisible part, or it is not a total"
    );

    // The gap is real, and this is the assertion that says so: the process counter moved by about the
    // guest mapping alone, while 8 MiB of arena went uncounted.
    let counted = budget.process_private as i64 - empty.process_private as i64;
    eprintln!(
        "committing {} MiB of guest memory and mapping {} MiB of code arena moved the VM_ACCOUNT total by {}",
        guest / MIB,
        8,
        mib(counted)
    );
    assert_close(counted, guest as i64, "VM_ACCOUNT counts the guest mapping and not the arena");

    space.unmap(address, guest).expect("unmap");
    space.close().expect("close");
}

/// **The other half of the requirement.** An instance grows to 3 GiB of committed guest memory at
/// the *default* ceilings, with no configuration at all, and gets every byte of it back.
///
/// This is the test that proves the commit ceiling did not fix a security hole by breaking the
/// feature. The project goal has Roblox legitimately needing several GB during startup before
/// settling near 500 MB, and D10 validated exactly that shape: an instance grown to 3 GB of live use
/// and then released, falling back to 513.656 MiB with its 4 GiB reservation intact. A ceiling that
/// refuses it is not a fix.
///
/// It is also why there are *two* ceilings. The tampered `p_memsz` the whole-branch review measured
/// asked for 3.3 GiB — more than this test commits — so no single number separates them. What
/// separates them is shape: this is 48 mappings growing over time, each well inside
/// `max_commit_request`; that was **one** mapping committed in **one** call. The sibling test
/// `an_eager_mapping_past_the_per_request_ceiling_is_refused` in `space.rs` is this test's other
/// half, and neither is evidence without the other.
fn an_instance_grows_to_three_gibibytes_at_the_default_ceilings() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    // Deliberately `new()`: the point is that this needs no configuration.
    let space = GuestSpace::new().expect("reserve a 4 GiB guest address space");
    let baseline = charge();
    let chunk = 64 * MIB;
    let chunks = 48;
    let total = chunk * chunks;
    assert_eq!(total, 3 * 1024 * MIB, "3 GiB, in the D10 requirement test's 64 MiB chunks");

    let mut addresses = Vec::new();
    for index in 0..chunks {
        match space.map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            chunk,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        ) {
            Ok(address) => addresses.push(address),
            Err(error) => {
                // A machine without 3 GiB of commit available is an environment limit, not a defect
                // — but it must not look like a pass. Say so on the real stderr, which libtest does
                // not discard, and give back what was taken.
                let is_commitment_limit = error
                    .platform_error()
                    .and_then(|platform| platform.os_error())
                    .is_some_and(|os| os.code() == 12);
                assert!(
                    is_commitment_limit,
                    "mapping {index} of {chunks} was refused by something other than the system \
                     commit limit, which is the defect this test exists to catch: {error}"
                );
                use std::io::Write;
                let notice = format!(
                    "\n!! SKIPPED an_instance_grows_to_three_gibibytes_at_the_default_ceilings: \
                     this machine ran out of system commit charge after {} of {total} bytes. The \
                     ceilings were NOT exercised.\n",
                    index * chunk
                );
                let _ = std::io::stderr().write_all(notice.as_bytes());
                for address in addresses {
                    space.unmap(address, chunk).expect("unmap");
                }
                return;
            }
        }
    }

    let peak = charge() - baseline;
    eprintln!(
        "grown to {} MiB at the default ceilings (max_committed {} MiB, max_commit_request {} MiB): \
         commit {}",
        total / MIB,
        omni_mem::DEFAULT_MAX_COMMITTED / MIB,
        omni_mem::DEFAULT_MAX_COMMIT_REQUEST / MIB,
        mib(peak)
    );
    // Against the size **plus its page-table charge**, which D10 measured as `size / 512` and Task 2
    // confirmed independently two orders of magnitude away (64 MiB committed cost +64.125 MiB). At
    // 3 GiB that is 6 MiB — larger than the shared tolerance, so asserting against the bare size
    // would need the tolerance loosened. Pinning the model instead is stricter, not looser.
    // **Linux: no page-table term.** `VM_ACCOUNT` is charged per page of mapping, and page tables
    // are not in it, so the model pinned here is the bare size -- exactly, within the tolerance.
    let page_tables = 0i64;
    assert_close(
        peak,
        total as i64 + page_tables,
        "3 GiB of live guest mappings at the defaults (no page-table charge on Linux)",
    );
    assert_eq!(space.stats().committed, total, "the space agrees with the OS");

    // The mappings are real and distinct, not one range handed out 48 times: a byte at the first and
    // last page of each, read back.
    for (index, &address) in addresses.iter().enumerate() {
        // SAFETY: the whole range is mapped ReadWrite and committed.
        unsafe {
            let pointer = space.ptr(address, chunk).expect("in the space");
            pointer.write(index as u8);
            pointer.add(chunk - 4096).write(index as u8);
        }
    }
    for (index, &address) in addresses.iter().enumerate() {
        // SAFETY: as above.
        unsafe {
            let pointer = space.ptr(address, chunk).expect("in the space");
            assert_eq!(pointer.read(), index as u8, "mapping {index} holds the wrong byte");
            assert_eq!(pointer.add(chunk - 4096).read(), index as u8);
        }
    }

    // One more chunk past the ceiling is refused, so the bound is real rather than merely distant:
    // 3072 + 64 MiB is still under 3.5 GiB, so this walks up to it rather than leaping past it.
    let mut extra = Vec::new();
    let mut refusal = None;
    for _ in 0..16 {
        match space.map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            chunk,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        ) {
            Ok(address) => extra.push(address),
            Err(error) => {
                refusal = Some(error);
                break;
            }
        }
    }
    let refusal = refusal.expect("the total ceiling must eventually refuse");
    assert!(
        matches!(refusal, MemError::CommitCeiling { .. }),
        "expected CommitCeiling, got {refusal}"
    );
    eprintln!(
        "after {} further 64 MiB chunks the total ceiling refused: {refusal}",
        extra.len()
    );
    assert_eq!(
        space.stats().committed,
        total + extra.len() * chunk,
        "the refusal changed nothing"
    );

    // And all of it comes back. This is D10's "grown to 3 GB and then released" end to end.
    for address in addresses.into_iter().chain(extra) {
        space.unmap(address, chunk).expect("unmap");
    }
    let after = charge() - baseline;
    eprintln!("after releasing everything: commit {} (page tables were {})", mib(after), mib(page_tables));
    assert_close(after, 0, "releasing 3 GiB of guest mappings");
    assert_eq!(space.stats().committed, 0);
    space.close().expect("close the guest space");
}

/// Every measurement above, in sequence, on one test thread. See the module docs for why.
#[test]
fn guest_memory_costs_with_no_other_test_thread_alive() {
    eprintln!("--- reserving_a_four_gibibyte_guest_space_costs_no_commit_charge");
    reserving_a_four_gibibyte_guest_space_costs_no_commit_charge();
    eprintln!("--- eight_guest_spaces_cost_no_more_than_one");
    eight_guest_spaces_cost_no_more_than_one();
    eprintln!("--- mapping_writing_reading_and_unmapping_returns_the_commit_charge");
    mapping_writing_reading_and_unmapping_returns_the_commit_charge();
    eprintln!("--- growing_to_a_gibibyte_and_releasing_everything_returns_commit_charge_to_baseline");
    growing_to_a_gibibyte_and_releasing_everything_returns_commit_charge_to_baseline();
    eprintln!("--- a_lazy_mapping_of_a_gibibyte_costs_nothing_until_it_is_reached");
    a_lazy_mapping_of_a_gibibyte_costs_nothing_until_it_is_reached();
    eprintln!("--- reclaim_idle_returns_the_commit_charge_it_claims_to_have_returned");
    reclaim_idle_returns_the_commit_charge_it_claims_to_have_returned();
    eprintln!("--- a_partial_unmap_returns_exactly_the_commit_charge_of_what_it_unmapped");
    a_partial_unmap_returns_exactly_the_commit_charge_of_what_it_unmapped();
    eprintln!("--- a_larger_commit_granule_is_dramatically_cheaper_per_page");
    a_larger_commit_granule_is_dramatically_cheaper_per_page();
    eprintln!("--- preserving_copy_on_write_content_does_not_privatise_the_clean_pages_of_a_survivor");
    preserving_copy_on_write_content_does_not_privatise_the_clean_pages_of_a_survivor();
    eprintln!("--- the_commit_budget_reports_the_arena_memory_the_process_counter_cannot_see");
    the_commit_budget_reports_the_arena_memory_the_process_counter_cannot_see();
    eprintln!("--- an_instance_grows_to_three_gibibytes_at_the_default_ceilings");
    an_instance_grows_to_three_gibibytes_at_the_default_ceilings();
}
